#pragma once

#include "Globals.h"

// 注册/卸载函数
HRESULT RegisterServer();
HRESULT UnregisterServer();

// 注册配置文件
HRESULT RegisterProfile();
HRESULT UnregisterProfile();

// 注册分类
HRESULT RegisterCategories();
HRESULT UnregisterCategories();
