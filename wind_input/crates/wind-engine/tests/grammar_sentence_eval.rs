//! 语法模型（bigram）的**整句**评测。
//!
//! ## 为什么不能用 `pinyin_eval` 验收 bigram
//!
//! `pinyin_eval` 的 A/B/C 三类都是**单个词**的测试（输入一个词的拼音、期望该词），
//! 而 bigram 打的是**词与词之间的转移分**——单词场景下根本不存在跨词转移。
//! 实测 `weight=1.0` 时 A/B/C 的 top-1 变化为 +0.10 / 0 / 0，纯属噪音级别。
//! 唯一涉及多词的 D 类又是「随机拼两个词」造出来的，本身不是自然语言。
//!
//! ⇒ 要看 bigram 有没有用，必须拿**真实的多词句子**测。本文件就是那个集合。
//!
//! ## 用法
//!
//! ```text
//! cargo test -p wind-engine --test grammar_sentence_eval -- --ignored --nocapture
//! ```
//!
//! 默认对比 `weight=0`（基线）与 `weight=1.0`，可用 `WIND_GRAM_WEIGHT` 改后者。
//! 需要 `build_dev/data` 与 `build_dev/data/schemas/pinyin/grammar/*.gram`。

use std::path::{Path, PathBuf};

use wind_config::Config;
use wind_engine::EngineManager;

const TOP_N: usize = 10;

/// 真实多词句子。**期望值是人工判定的「自然中文」**，不是从词库合成的。
///
/// 选材原则：每条都要么在设计文档 §1 被点名，要么是日常高频表达——
/// 也就是「用户真会这么打、且上下文能帮上忙」的场景。
const CASES: &[(&str, &str)] = &[
    // —— 设计文档 §1 表格点名的定点 ——
    ("sixiang", "思想"),
    ("nihao", "你好"),
    // —— 双词搭配：上下文应能定夺 ——
    ("qihoutezheng", "气候特征"),
    ("zhengquangongsi", "证券公司"),
    ("xinlishang", "心理上"),
    ("dulizizhu", "独立自主"),
    ("jianyixiugai", "建议修改"),
    // —— 短句 ——
    ("woshizhongguoren", "我是中国人"),
    ("jintiantianqihenhao", "今天天气很好"),
    ("womenyiqiquchifan", "我们一起去吃饭"),
    ("xiexienidebangzhu", "谢谢你的帮助"),
    ("zhegewentihenzhongyao", "这个问题很重要"),
    ("qingwenxianzaijidian", "请问现在几点"),
    ("womenxuyaogengduoshijian", "我们需要更多时间"),
    ("tazhengzaikanshu", "他正在看书"),
    ("zhonghuarenmingongheguo", "中华人民共和国"),
    ("jisuanjikexue", "计算机科学"),
    ("rengongzhineng", "人工智能"),
    ("gongzuobaogao", "工作报告"),
    ("chifanlema", "吃饭了吗"),
    ("zhendehenbucuo", "真的很不错"),
    // —— 同音消歧：**正是 bigram 该拿分的地方**，靠上下文才分得出 ——
    ("mingtianzaijian", "明天再见"),            // 再见 / 在建
    ("shenghuozhongdexiaoshi", "生活中的小事"), // 小事 / 小时
    ("woyaoqushangban", "我要去上班"),
    ("tadeyisijiushi", "他的意思就是"),
    ("zheshiyigehaobanfa", "这是一个好办法"),
    // —— 更多日常整句：扩大样本以便看出基线本来就答错的那些 ——
    ("wobuzhidaozenmeban", "我不知道怎么办"),
    ("nizuotianqunalile", "你昨天去哪里了"),
    ("womingtianyaokaihui", "我明天要开会"),
    ("zhebenshuhenyouyisi", "这本书很有意思"),
    ("wodepengyoulaile", "我的朋友来了"),
    ("yijingwanchengle", "已经完成了"),
    ("xuyaoduoshaoqian", "需要多少钱"),
    ("qingnigaosuwo", "请你告诉我"),
    ("tashuodehendui", "他说得很对"),
    ("zheyangzuobutaihao", "这样做不太好"),
    ("womenzaiyiqigongzuo", "我们在一起工作"),
    ("zhegeshihouyinggai", "这个时候应该"),
    ("kanwandianyingyihou", "看完电影以后"),
    ("zuotianwanshangxiayu", "昨天晚上下雨"),
    ("womendeshijianbugou", "我们的时间不够"),
    ("zhegeshijianhenzhongyao", "这个事件很重要"),
    ("gongsideguidingshi", "公司的规定是"),
    ("qingtijiaoshenqing", "请提交申请"),
    ("xiawuliangdiankaishi", "下午两点开始"),
    ("tazaijiaxiuxi", "他在家休息"),
    ("womenyinggaizenmezuo", "我们应该怎么做"),
    ("zhejianshiqingbunan", "这件事情不难"),
    ("nihaishiyaoxiaoxin", "你还是要小心"),
    ("dajiadouzhidaole", "大家都知道了"),
    // ═══ 以下为 2026-08-15 扩充（50 → 200 条）═══
    //
    // 50 条太小：一条样本翻转就是 ±2%，而 bigram 的真实效应量本就在 ±2% 附近，
    // 信号完全淹没在噪声里。扩到 200 条后单条权重降到 0.5%，才谈得上「测得出」。
    //
    // 选材仍守原则：**期望值是人工判定的自然中文**，且必须是词库打得出的常用表达；
    // 生僻词即便 bigram 再准也打不出来，放进来只会制造恒定的失败噪声。

    // —— 「的/得/地」：最高频的同音三分，且只能靠上下文定夺 ——
    ("nishuodeduibudui", "你说得对不对"),
    ("tazuodefeichanghao", "他做得非常好"),
    ("wodexiangfashi", "我的想法是"),
    ("manmandezoulai", "慢慢地走来"),
    ("gaoxingdetiaoqilai", "高兴地跳起来"),
    ("tapaodehenkuai", "他跑得很快"),
    ("zhegeshiwodedongxi", "这个是我的东西"),
    ("renzhendeting", "认真地听"),
    // —— 「在/再」：同音且都高频 ——
    ("womingtianzaishuo", "我明天再说"),
    ("tazhengzaigongzuo", "他正在工作"),
    ("zaijianwodepengyou", "再见我的朋友"),
    ("womenzaijiadengni", "我们在家等你"),
    ("zaishuoyibian", "再说一遍"),
    ("xianzaizainali", "现在在哪里"),
    // —— 「那/哪」：疑问与指示的同音对立 ——
    ("younaxieyaoqiu", "有哪些要求"),
    ("nagerenshishui", "那个人是谁"),
    ("naliyoumaide", "哪里有卖的"),
    ("nashiwodeshu", "那是我的书"),
    ("nizainalishangban", "你在哪里上班"),
    // —— 「事/是/时」：三向同音，整句里最常见的错解 ——
    ("zheshijianshiqing", "这是件事情"),
    ("zhegeshiqinghenji", "这个事情很急"),
    ("shijieshangzuikuai", "世界上最快"),
    ("dangshiwobuzhidao", "当时我不知道"),
    ("youshijiaowo", "有事叫我"),
    ("zhenshibuhaoyisi", "真是不好意思"),
    ("gongzuoshijianjieshu", "工作时间结束"),
    // —— 「他/她/它」与人称 ——
    ("tashiwodetongshi", "他是我的同事"),
    ("tamenyijingzoule", "他们已经走了"),
    ("womenlianggehaopengyou", "我们两个好朋友"),
    // —— 日常高频整句 ——
    ("qingshaodengyixia", "请稍等一下"),
    ("womashanghuilai", "我马上回来"),
    ("jintianwohenmang", "今天我很忙"),
    ("mingtianyoushijianma", "明天有时间吗"),
    ("nizhendetaihaole", "你真的太好了"),
    ("meishibiekeqi", "没事别客气"),
    ("zhenduibuqiwolaiwanle", "真对不起我来晚了"),
    ("ganxienindebangzhu", "感谢您的帮助"),
    ("qidainindehuifu", "期待您的回复"),
    ("zhuninshentijiankang", "祝您身体健康"),
    ("womenmingtianjian", "我们明天见"),
    ("haojiubujianle", "好久不见了"),
    ("zuijinzenmeyang", "最近怎么样"),
    ("wozaikaihui", "我在开会"),
    ("bushiwodecuo", "不是我的错"),
    ("zheyangkeyima", "这样可以吗"),
    ("dangranmeiwenti", "当然没问题"),
    ("wozhidaolexiexie", "我知道了谢谢"),
    // —— 工作 / 事务类整句 ——
    ("wodezhuyaogongzuo", "我的主要工作"),
    ("zheshiwodezeren", "这是我的责任"),
    ("tigaogongzuoxiaolv", "提高工作效率"),
    ("jiejuezhegewenti", "解决这个问题"),
    ("anpaiyixiashijian", "安排一下时间"),
    ("qingniquerenyixia", "请你确认一下"),
    ("womenxuyaotaolun", "我们需要讨论"),
    ("zhegefanganbucuo", "这个方案不错"),
    ("mingtianjiaobaogao", "明天交报告"),
    ("huiyituidaoxiazhou", "会议推到下周"),
    ("qingchakanfujian", "请查看附件"),
    ("yijingfageinile", "已经发给你了"),
    ("wohuijixuguanzhu", "我会继续关注"),
    ("zhegerenwuwanchengle", "这个任务完成了"),
    ("xuyaonideyijian", "需要你的意见"),
    // —— 长句（8 字以上，跨词转移最多）——
    ("womenxuyaoyigexindefangan", "我们需要一个新的方案"),
    ("zhegexiangmuyijingwanchengle", "这个项目已经完成了"),
    ("tamenmingtianhuilaikaihui", "他们明天回来开会"),
    ("zhejianshiqingxuyaoshijian", "这件事情需要时间"),
    ("nikeyigaosuwodianhuama", "你可以告诉我电话吗"),
    ("womendeyijianbutaiyizhi", "我们的意见不太一致"),
    ("zhegewentibijiaofuza", "这个问题比较复杂"),
    ("tashuotayaoqubeijing", "他说他要去北京"),
    ("womenzaigongyuanjianmian", "我们在公园见面"),
    ("jintianwanshangyiqichifan", "今天晚上一起吃饭"),
    ("mingtianzaoshangbadianchufa", "明天早上八点出发"),
    ("wojuedezheyangbutaihao", "我觉得这样不太好"),
    ("nishifoukeyibangwoyixia", "你是否可以帮我一下"),
    ("womenyinggaizaikaolvkaolv", "我们应该再考虑考虑"),
    ("zhebenshuwoyijingkanwanle", "这本书我已经看完了"),
    ("tadegongzuotaimangle", "他的工作太忙了"),
    ("womenyaohaohaoxuexi", "我们要好好学习"),
    ("nishuodehenyoudaoli", "你说得很有道理"),
    ("tamenyijingchufale", "他们已经出发了"),
    ("womendejihuayaogaibian", "我们的计划要改变"),
    ("qingdajiazhuyianquan", "请大家注意安全"),
    ("zhegedifangwolaiguo", "这个地方我来过"),
    ("wobuzhidaotazainali", "我不知道他在哪里"),
    ("mingtiantianqihuizenmeyang", "明天天气会怎么样"),
    ("womenxiazhouzaijianmian", "我们下周再见面"),
];

/// 扩充第二批：数量词、时间、固定搭配、易被字级模型打散的词组。
///
/// 与 [`CASES`] 分开只是为了可读，实际评测时两者合并（见 `all_cases`）。
const CASES_EXT: &[(&str, &str)] = &[
    // —— 数量词 / 时间：量词与数字最易被同音字冲掉 ——
    ("mingtianxiawusandian", "明天下午三点"),
    ("yigongsanbaikuaiqian", "一共三百块钱"),
    ("dengleyigexiaoshi", "等了一个小时"),
    ("zoulesanshifenzhong", "走了三十分钟"),
    ("maileliangjinpingguo", "买了两斤苹果"),
    ("huafeiliangtianshijian", "花费两天时间"),
    ("yigongyoushigeren", "一共有十个人"),
    ("diyicikanjian", "第一次看见"),
    ("liangnianqiandeshiqing", "两年前的事情"),
    ("sangeyuehou", "三个月后"),
    ("shangwujiudiankaishi", "上午九点开始"),
    ("wanshangbadianjieshu", "晚上八点结束"),
    ("yitianyici", "一天一次"),
    ("bannianyihoucaizhidao", "半年以后才知道"),
    ("jitianneihuifu", "几天内回复"),
    // —— 固定搭配 / 四字词：整体性强，最能看出模型会不会打散词组 ——
    ("shishiqiushi", "实事求是"),
    ("yirujiwang", "一如既往"),
    ("quanliyifu", "全力以赴"),
    ("renzhenfuze", "认真负责"),
    ("zonghekaolv", "综合考虑"),
    ("jiaqiangguanli", "加强管理"),
    ("tigaozhiliang", "提高质量"),
    ("jiejuefangshi", "解决方式"),
    ("chongfenzhunbei", "充分准备"),
    ("jijiyingdui", "积极应对"),
    ("baochilianxi", "保持联系"),
    ("zhuyianquan", "注意安全"),
    ("gongtongnuli", "共同努力"),
    ("xiangguanbumen", "相关部门"),
    ("jutitiaozheng", "具体调整"),
    // —— 易被字级模型打散的多字词组（bgc 的已知弱项）——
    ("shehuizhuyi", "社会主义"),
    ("jingjifazhan", "经济发展"),
    ("kejichuangxin", "科技创新"),
    ("jiaoyugaige", "教育改革"),
    ("huanjingbaohu", "环境保护"),
    ("yiliaobaozhang", "医疗保障"),
    ("wenhuachuancheng", "文化传承"),
    ("guojiazhengce", "国家政策"),
    ("chengshijianshe", "城市建设"),
    ("nongyeshengchan", "农业生产"),
    ("jinrongshichang", "金融市场"),
    ("qiyeguanli", "企业管理"),
    ("renlicaiyuan", "人力资源"),
    ("shujufenxi", "数据分析"),
    ("wangluoanquan", "网络安全"),
    // —— 口语 / 语气结尾 ——
    ("nikeyibangwokankanma", "你可以帮我看看吗"),
    ("zhegezenmeyongya", "这个怎么用呀"),
    ("womenzoubaxianzai", "我们走吧现在"),
    ("haodejiuzheyangba", "好的就这样吧"),
    ("nizhendequedingma", "你真的确定吗"),
    ("shibushizhende", "是不是真的"),
    ("keyishishima", "可以试试吗"),
    ("yaobuyaoyiqiqu", "要不要一起去"),
    ("zenmehuizheyang", "怎么会这样"),
    ("weishenmebugaosuwo", "为什么不告诉我"),
];

/// 两批用例合并后的全集。分成两个常量只是为了可读，评测一律走这里。
fn all_cases() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    CASES.iter().chain(CASES_EXT.iter())
}

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn manager(dir: &Path, weight: f64) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    cfg.schema.pinyin.grammar.weight = weight;
    // 换模型：bgc（字级 2-gram）与 bgw（词级）行为差别很大，标定时要能一键切换。
    if let Ok(m) = std::env::var("WIND_GRAM_MODEL") {
        cfg.schema.pinyin.grammar.model = m;
    }
    EngineManager::new(&cfg, Some(dir))
}

#[test]
#[ignore = "整句评测：依赖 build_dev 真实词库与 .gram 模型。用 --ignored 显式运行"]
fn grammar_sentence_report() {
    let Some(dir) = data_dir() else {
        eprintln!("!!! 跳过：build_dev 拼音词库不存在");
        return;
    };
    let gram = dir.join("schemas/pinyin/grammar");
    if !gram.exists() {
        eprintln!(
            "!!! 跳过：找不到 {}。\n\
             !!! 获取：curl -L -o zh-hans-bgc.gram \
             https://github.com/lotem/rime-octagram-data/raw/hans/zh-hans-t-essay-bgc.gram",
            gram.display()
        );
        return;
    }

    let weight = std::env::var("WIND_GRAM_WEIGHT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);

    let base = manager(&dir, 0.0);
    let with = manager(&dir, weight);

    let (mut base_hit, mut with_hit) = (0usize, 0usize);
    let mut changed: Vec<(&str, &str, String, String)> = Vec::new();

    for &(input, expect) in all_cases() {
        let b = base.convert_with("pinyin", input, TOP_N).candidates;
        let w = with.convert_with("pinyin", input, TOP_N).candidates;
        let bt = b.first().map(|c| c.text.clone()).unwrap_or_default();
        let wt = w.first().map(|c| c.text.clone()).unwrap_or_default();
        if bt == expect {
            base_hit += 1;
        }
        if wt == expect {
            with_hit += 1;
        }
        if bt != wt {
            changed.push((input, expect, bt, wt));
        }
    }

    let n = all_cases().count();
    println!("\n=== 整句评测 (weight={weight}) ===");
    println!("样本 {n}");
    println!(
        "基线   top-1 命中 {base_hit}/{n} = {:.1}%",
        base_hit as f64 * 100.0 / n as f64
    );
    println!(
        "接模型 top-1 命中 {with_hit}/{n} = {:.1}%   ({:+})",
        with_hit as f64 * 100.0 / n as f64,
        with_hit as i64 - base_hit as i64
    );

    println!("\n--- 首选发生变化的样本 ({}) ---", changed.len());
    for (input, expect, bt, wt) in &changed {
        // 标注这次改动是修好了、弄坏了、还是两边都不对
        let tag = match (bt == expect, wt == expect) {
            (false, true) => "修好",
            (true, false) => "弄坏",
            _ => "都错",
        };
        println!("  [{tag}] {input:<26} 期望 {expect:<14} 基线 {bt:<14} 新 {wt}");
    }

    println!("\n--- 两边一致的样本里仍未命中的 ---");
    for &(input, expect) in all_cases() {
        let b = base.convert_with("pinyin", input, TOP_N).candidates;
        let bt = b.first().map(|c| c.text.clone()).unwrap_or_default();
        let w = with.convert_with("pinyin", input, TOP_N).candidates;
        let wt = w.first().map(|c| c.text.clone()).unwrap_or_default();
        if bt == wt && bt != expect {
            println!("  {input:<26} 期望 {expect:<14} 实得 {bt}");
        }
    }
}
