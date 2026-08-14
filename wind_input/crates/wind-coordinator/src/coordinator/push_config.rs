//! push 通道的配置/状态推送：activation status、各配置帧、状态更新。
//!（coordinator 子模块，自 coordinator.rs 平移，纯搬运。）

use super::*;

impl Coordinator {
    /// `client_token` = 触发本次 activation 的客户端 token（高 32 位 = PID，
    /// BinaryProtocol.h PushTokenHandshake 约定）。hostRenderAvail 位**必须**按
    /// 事件源 PID 查白名单（对齐 Go PushActivationStatusToActiveClient(status, processID)）——
    /// 不能用全局焦点槽：开始菜单弹出会连带激活 StartMenuExperienceHost 等兄弟进程，
    /// 其激活事件若污染全局槽，推给 SearchHost 的 avail 位会错置 0，触发 DLL
    /// 「flag missing after reconnect」销毁重建循环（真机踩坑）。
    pub(super) fn push_activation_status(&self, client_token: u64) {
        let s = self.build_status();
        debug!(
            "push_activation_status: chinese={} key_down={:?} key_up={:?}",
            s.chinese_mode, s.key_down_hotkeys, s.key_up_hotkeys
        );
        #[cfg(windows)]
        let host_render_avail = {
            let pid = (client_token >> 32) as u32;
            pid != 0
                && self
                    .host_render()
                    .map(|m| m.is_process_whitelisted(pid))
                    .unwrap_or(false)
        };
        #[cfg(not(windows))]
        let host_render_avail = {
            let _ = client_token;
            false
        };
        let encoded = wind_ipc::codec::encode_activation_status_push(
            s.chinese_mode,
            s.full_width,
            s.chinese_punct,
            s.toolbar_visible,
            s.caps_lock,
            host_render_avail,
            &s.key_down_hotkeys,
            &s.key_up_hotkeys,
            &s.icon_label,
        );
        // 定向投递给事件源客户端（精确 token 匹配）。push_to_active 实为广播——广播会把
        // 按别的进程计算的 hostRenderAvail 位污染给无关客户端（真机踩坑：开始菜单弹出时
        // StartMenuExperienceHost 等兄弟实例的激活推送被 SearchHost 收到，avail=0 触发
        // Band 窗口销毁重建循环）。事件源无 push 连接时丢弃，绝不兜底转发。
        if client_token != 0 {
            if !self.push_server.push_to_token(client_token, &encoded) {
                debug!("activation push: 事件源 token 无 push 连接，丢弃（防污染不广播）");
            }
        } else {
            // 无 token 的旧路径（不应出现于当前 DLL）：保持原广播行为
            self.push_server.push_to_active(&encoded);
        }
    }

    /// push 客户端完成 token 握手后的补推握手（仅 Windows；由 main.rs 注册到 PushServer）。
    /// 场景：服务重启后，白名单受限宿主（SearchHost 等 locked/transient DocMgr）重连时
    /// 既不发 focus_gained（被 DLL OnSetFocus 跳过）也不重发 IME_ACTIVATED——没有任何
    /// activation push 会到达，DLL 的 host 窗口挂着死 SHM 永不重新 setup（真机踩坑：
    /// 服务重启后概率性停留普通渲染）。此处对白名单 pid 定向补推一帧 activation status
    /// （avail=1），触发 C++ ApplyActivationStatusResponse → _EnsureHostRenderSetup
    /// （forceRefresh）→ 重新握手 setup。非白名单进程不推，零影响。
    #[cfg(windows)]
    pub fn on_push_client_connected(&self, client_token: u64) {
        let pid = (client_token >> 32) as u32;
        if pid == 0 {
            return;
        }

        // 推送英文自动配对配置到新连接的客户端（不受 host-render 白名单限制，
        // 所有 TSF 实例都需要收到此配置才能在英文模式下正确处理标点配对）。
        self.push_english_pair_config(client_token);
        self.push_jump_out_keys_config(client_token); // 配对跳出键（英文模式跳出 + 中文转发放行）
        self.push_password_suppress_config(client_token); // 密码框抑制策略（DLL 本地吃键门控）
        self.push_custom_en_punct_config(client_token); // 英半列自定义标点：DLL 据此吃键转发
        self.push_pair_state_ttl_config(client_token); // 配对状态时效（DLL 侧闸门据此判陈旧）
        // 诊断采集开关：DLL 每次重连都从默认值（关）起步，握手不推则 HUD 开着也收不到
        // 新连接宿主的快照——而最需要它的 SearchHost 恰恰是最常重连的那类。
        self.push_diag_snapshot_config(client_token);

        let Some(mgr) = self.host_render() else {
            return;
        };
        if !mgr.is_process_whitelisted(pid) {
            return;
        }
        tracing::info!("push 客户端注册补推 activation（host-render 白名单宿主）pid={pid}");
        self.push_activation_status(client_token);
    }

    /// 指定 PID 的进程是否启用符号自动配对（per-app 规则，未配则跟随全局）。
    ///
    /// ⚠ **按 PID 直查规则表，绝不走 `active_compat` 焦点槽**：本函数的调用方是推送路径，
    /// 目标客户端未必是当前焦点进程（新客户端握手、配置变更广播都会推给后台进程）。
    /// 拿焦点槽的值会把焦点应用的规则套到别人头上——同 `host_render` 的既有纪律。
    pub(super) fn auto_pair_allowed_for_pid(&self, pid: u32) -> bool {
        if pid == 0 {
            return true;
        }
        let name = {
            let cached = self
                .pid_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&pid)
                .cloned();
            cached.unwrap_or_else(|| process_name(pid))
        };
        if name.is_empty() {
            return true;
        }
        self.app_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_rule(&name)
            .and_then(|r| r.auto_pair)
            .unwrap_or(true)
    }

    /// 推送英文自动配对配置到指定客户端（或逐个推给所有活跃客户端）。
    ///
    /// 这是 per-app 自动配对开关的**第三条**消费通路：纯英文模式的配对完全由 C++ 侧
    /// `_englishPairEngine` 处理，那些标点键根本到不了协调器，只关另两条的话「切到英文
    /// 模式又配上了」。故 enabled 必须按**目标进程**现算，不能全局广播同一个值。
    pub fn push_english_pair_config(&self, client_token: u64) {
        let rt = self.rt();
        let make = |token: u64| {
            let pid = (token >> 32) as u32;
            let enabled = rt.config.input.auto_pair.english && self.auto_pair_allowed_for_pid(pid);
            let value = wind_ipc::codec::encode_english_pairs_value(enabled, &rt.en_pairs);
            wind_ipc::codec::encode_sync_config(
                wind_ipc::protocol::CONFIG_KEY_ENGLISH_PAIRS,
                &value,
            )
        };
        if client_token != 0 {
            self.push_server
                .push_to_token(client_token, &make(client_token));
        } else {
            self.push_server.push_per_client(make);
        }
    }

    /// 下发配对状态时效给 DLL。吃键闸门（`_pairPendingDepth`）在 DLL 侧，它必须能本地判定
    /// 状态是否陈旧——只有协调器过期而 DLL 照吃跳出键的话，协调器回 PassThrough 已太晚
    /// （「吃了再吐」丢键）。故 TTL 以 DLL 侧判据为准，此处只推阈值。
    pub fn push_pair_state_ttl_config(&self, client_token: u64) {
        let secs = self.rt().config.input.auto_pair.state_ttl_secs;
        let value = wind_ipc::codec::encode_pair_state_ttl_value(secs);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_PAIR_STATE_TTL,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 下发密码框抑制策略开关给 DLL。DLL 据此 + 自身持有的 InputScope 掩码在
    /// `OnTestKeyDown` 本地判定是否放行；判据两侧必须一致（见 `apply_input_diag` 与
    /// C++ `IsPasswordSuppressActive`），漂移即「吃了再吐」丢键。
    /// 开关是会话级运行时态（右键菜单「高级」可切），故握手时与每次切换后都要推。
    pub fn push_password_suppress_config(&self, client_token: u64) {
        let enabled = self
            .password_suppress_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let value = wind_ipc::codec::encode_password_suppress_value(enabled);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_PASSWORD_SUPPRESS,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 下发「英文模式下 DLL 需吃键转发」的源字符集合给 DLL。两个来源合成一份推送：
    ///   - 配了**英半列自定义**的键（`wind_punct::custom_english_punct_chars`）；
    ///   - 开了 `symbol.english_mode` 时的**英文智能符号参与集**（`english_smart_source_chars`）。
    ///
    /// 英文模式（非全角）下 DLL 默认直接透传标点键、引擎收不到，上面两件事因此都无从发生；
    /// DLL 据此集合精确吃下这些键并转发（集合为空 = 完全保持历史行为）。**吃键集必须 ⊆ 出字集**：
    /// 出字方 `handle_english_custom_punct` 与本推送共用 `rt().custom_en_punct_chars` 作判据，
    /// 同源即不会漂移；两侧一旦不一致就是「吃了再吐」丢键（Chrome/Electron 不回退合成 WM_CHAR）。
    /// 集合内没配英半自定义的键会出原样 ASCII（与透传等价），故并入是安全的。
    pub fn push_custom_en_punct_config(&self, client_token: u64) {
        // BTreeSet 迭代天然有序 → 推送字节可复现（与 jump_out_keys 排序同理）。
        let chars: Vec<char> = self.rt().custom_en_punct_chars.iter().copied().collect();
        let value = wind_ipc::codec::encode_custom_en_punct_value(&chars);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_CUSTOM_EN_PUNCT,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 推送配对跳出键（VK 码集合）到 TSF 客户端。TSF 英文模式配对直接据此跳出；
    /// 中文模式据此在「有待跳出配对」时放行转发（真正裁决仍在协调器）。
    pub fn push_jump_out_keys_config(&self, client_token: u64) {
        let rt = self.rt();
        // HashSet 迭代序不稳定，排序保证推送字节可复现。
        let mut vks: Vec<u32> = rt.jump_out_keys.iter().copied().collect();
        vks.sort_unstable();
        let value = wind_ipc::codec::encode_jump_out_keys_value(rt.jump_out_on_right_symbol, &vks);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_JUMP_OUT_KEYS,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// macOS：把命令直通车按键合成帧（CmdKeyTap/Seq/Hold/Release/Type）推给活跃 `.app`。
    /// 服务进程（LaunchAgent）无辅助功能授权无法 post CGEvent，改由 `.app` 侧 KeySynthesizer
    /// 合成（`.app` 有授权）。只投活跃前台客户端，与 commit 同队列保证与 type() 上屏文本的顺序。
    #[cfg(target_os = "macos")]
    pub(crate) fn push_cmdbar_key_frame(&self, encoded: &[u8]) {
        self.push_server.push_commit_to_active(encoded);
    }

    /// macOS 的 open/proc.run/设置均改为进程内执行或 CmdOpenSettings，不再经此 IPC，故仅非 macOS。
    ///
    /// `dir` = 被启动进程的工作目录（空串 = 不指定，由 TSF 侧沿用调用进程当前目录）；
    /// `verb` / `show` = ShellExecute 的动词与初始窗口状态（空串 = open / normal）。
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn push_shell_exec(
        &self,
        target: &str,
        params: &str,
        dir: &str,
        verb: &str,
        show: &str,
    ) {
        let encoded = wind_ipc::codec::encode_shell_exec(target, params, dir, verb, show);
        // 带副作用操作（启动/激活外部程序）只投给活跃（前台）客户端，与 push_commit 语义一致。
        // 若广播全部客户端，多个后台 TSF 进程会竞相 ShellExecuteW，非前台进程启动的 wind_setting
        // 第二实例无前台权限，其 SetForegroundWindow 失败，导致窗口有较大概率停在后台。
        self.push_server.push_commit_to_active(&encoded);
    }

    pub(crate) fn push_state_update(&self) {
        let s = self.build_status();
        let encoded = wind_ipc::codec::encode_state_push(
            s.chinese_mode,
            s.full_width,
            s.chinese_punct,
            s.toolbar_visible,
            s.caps_lock,
            &s.icon_label,
        );
        self.push_server.push_to_active(&encoded);

        // 图标位图与状态推送同源同时机：DLL 收到本次推送后会 OnUpdate(TF_LBI_ICON)，
        // 系统随即回调 GetIcon 去读 SHM——那时新位图必须已经在里面。
        #[cfg(all(feature = "desktop-ui", windows))]
        self.publish_langbar_icon(&s);
    }
}
