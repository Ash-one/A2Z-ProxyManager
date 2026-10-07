#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
A2Z-ProxyManager Z.AI (ZCode Plan) 每日免费福利自动化领取与验证脚本
- 目标：自动检索并领取限时福利活动套餐（1亿 Token，专供 GLM-5.3-Flash）
- 流程：
    1. 读取本地 ~/.antigravity_tools/gui_config.json 凭证与设备指纹
    2. 检查 127.0.0.1:8045 后端健康状态
    3. 轮询 /api/zcode/plan/claim/preview 探测上线的活动套餐
    4. 调用 captcha_node/solver.js 自动生成阿里云无痕验证码
    5. 模拟前端“领取”行为，调用 /api/zcode/plan/claim 提交领取
    6. 重新查询 /api/zcode/plan/quota 核实验证最新配额
"""

import sys
import os
import json
import time
import subprocess
import urllib.request
import urllib.error
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

CONFIG_PATH = Path.home() / ".antigravity_tools" / "gui_config.json"
REPO_DIR = Path(__file__).resolve().parent.parent
SOLVER_PATH = REPO_DIR / "captcha_node" / "solver.js"

def log(msg: str) -> None:
    now = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{now}] {msg}", flush=True)

def load_config() -> Dict[str, Any]:
    if not CONFIG_PATH.exists():
        raise FileNotFoundError(f"配置文件未找到: {CONFIG_PATH}")
    with open(CONFIG_PATH, "r", encoding="utf-8") as f:
        return json.load(f)

def make_request(url: str, api_key: str, payload: Optional[Dict[str, Any]] = None, timeout: int = 15) -> Tuple[int, Any]:
    headers = {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {api_key}"
    }
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    req = urllib.request.Request(url, data=data, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = resp.read().decode("utf-8")
            return resp.status, json.loads(body) if body else {}
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8")
        try:
            parsed = json.loads(body)
        except Exception:
            parsed = {"raw": body}
        return e.code, parsed
    except Exception as e:
        return 500, {"error": str(e)}

def solve_captcha() -> str:
    if not SOLVER_PATH.exists():
        raise FileNotFoundError(f"验证码求解器不存在: {SOLVER_PATH}")
    log("正在通过 Node happy-dom 自动求解阿里云无痕验证码...")
    proc = subprocess.run(
        ["node", str(SOLVER_PATH), "11xygtvd", "cn", "no8xfe"],
        cwd=str(SOLVER_PATH.parent),
        capture_output=True,
        text=True,
        timeout=15
    )
    if proc.returncode != 0:
        raise RuntimeError(f"验证码求解失败 (code {proc.returncode}): {proc.stderr}")
    
    for line in proc.stdout.splitlines():
        if line.startswith("VERIFY_PARAM="):
            param = line.split("VERIFY_PARAM=", 1)[1].strip()
            if param:
                log("验证码求解成功！已获得新鲜校验参数。")
                return param
    raise RuntimeError(f"未能从求解器输出中提取 VERIFY_PARAM: {proc.stdout}")

def query_quota(base_url: str, api_key: str, account: Dict[str, Any]) -> Tuple[int, Any]:
    payload = {
        "zcode_jwt": account.get("key", ""),
        "device_profile": account.get("device_profile")
    }
    return make_request(f"{base_url}/api/zcode/plan/quota", api_key, payload)

def check_preview(base_url: str, api_key: str, account: Dict[str, Any]) -> Tuple[int, Any]:
    payload = {
        "zcode_jwt": account.get("key", ""),
        "device_profile": account.get("device_profile")
    }
    return make_request(f"{base_url}/api/zcode/plan/claim/preview", api_key, payload)

def claim_plan(base_url: str, api_key: str, account: Dict[str, Any], plan_id: str, verify_param: str) -> Tuple[int, Any]:
    payload = {
        "zcode_jwt": account.get("key", ""),
        "device_profile": account.get("device_profile"),
        "plan_id": plan_id,
        "verify_param": verify_param,
        "region": "cn"
    }
    return make_request(f"{base_url}/api/zcode/plan/claim", api_key, payload)

def run_claim_process(poll_seconds: int = 180, interval: int = 10) -> bool:
    cfg = load_config()
    proxy_cfg = cfg.get("proxy", {})
    port = proxy_cfg.get("port", 8045)
    api_key = proxy_cfg.get("api_key", "")
    base_url = f"http://127.0.0.1:{port}"

    zai_keys: List[Dict[str, Any]] = proxy_cfg.get("zai", {}).get("keys", [])
    jwt_accounts = [k for k in zai_keys if isinstance(k, dict) and (k.get("provider") == "zcode_plan" or k.get("mode") == "jwt")]
    if not jwt_accounts:
        log("错误：未找到有效的 ZCode Plan (JWT) 账号配置！")
        return False

    account = jwt_accounts[0]
    label = account.get("label") or account.get("user_email") or "unknown"
    log(f"目标账号: {label} (ID: {account.get('account_id')})")

    # 1. 查询当前额度
    log("正在查询当前账号已有套餐配额...")
    status, quota_data = query_quota(base_url, api_key, account)
    if status == 200 and isinstance(quota_data, dict):
        plans: List[Dict[str, Any]] = quota_data.get("plans", [])
        balances: List[Dict[str, Any]] = quota_data.get("balances", [])
        log(f"当前生效套餐数量: {len(plans)}")
        for p in plans:
            if isinstance(p, dict):
                ends_at = p.get("ends_at", 0)
                ends_at_str = datetime.fromtimestamp(ends_at).strftime("%Y-%m-%d %H:%M:%S") if ends_at else "永不过期"
                log(f" - 套餐: {p.get('name')} ({p.get('plan_id')}), 截止时间: {ends_at_str}")
        for b in balances:
            if isinstance(b, dict):
                log(f" - 余额: {b.get('show_name')} 可用 {b.get('available_units')}/{b.get('total_units')} {b.get('unit_type')}")
    else:
        log(f"查询配额失败 (HTTP {status}): {quota_data}")

    # 2. 轮询可领套餐
    log(f"开始探测可领取套餐 (最多持续 {poll_seconds} 秒，间隔 {interval} 秒)...")
    start_time = time.time()
    found_plans: List[Dict[str, Any]] = []

    while time.time() - start_time < poll_seconds:
        status, preview_data = check_preview(base_url, api_key, account)
        if status == 200 and isinstance(preview_data, list) and len(preview_data) > 0:
            found_plans = preview_data
            log(f"成功发现可领取活动套餐: {len(found_plans)} 个！")
            break
        elif status == 200:
            elapsed = int(time.time() - start_time)
            log(f"当前暂无上线活动套餐 (耗时 {elapsed}s)，等待重试...")
        else:
            log(f"预览接口返回异常 (HTTP {status}): {preview_data}")
        time.sleep(interval)

    if not found_plans:
        log("轮询超时：暂未发现上线的活动套餐。")
        return False

    # 3. 逐个执行领取
    success_count = 0
    for plan in found_plans:
        if not isinstance(plan, dict):
            continue
        plan_id = str(plan.get("plan_id", ""))
        plan_name = str(plan.get("name") or plan_id)
        log(f"准备领取套餐: {plan_name} (ID: {plan_id})")

        # 求解验证码
        try:
            verify_param = solve_captcha()
        except Exception as e:
            log(f"获取验证码失败: {e}")
            continue

        # 发起领取
        log(f"正在提交领取请求: {plan_id}...")
        c_status, c_res = claim_plan(base_url, api_key, account, plan_id, verify_param)
        log(f"领取接口响应 (HTTP {c_status}): {c_res}")

        if isinstance(c_res, dict):
            if c_res.get("ok"):
                log(f"🎉 套餐 {plan_name} 领取成功！")
                success_count += 1
            elif c_res.get("code") == 1003:
                log(f"⚠️ 套餐 {plan_name} 今日已经领取过，无需重复领取。")
            elif c_res.get("code") == 1005:
                next_at = c_res.get("next_at_ms")
                next_str = datetime.fromtimestamp(float(next_at) / 1000.0).strftime("%Y-%m-%d %H:%M:%S") if next_at else "下一周期"
                log(f"⚠️ 套餐 {plan_name} 今日名额已用完，下次开放时间: {next_str}")
            elif c_res.get("code") == 3007:
                log(f"❌ 验证码校验失败，尝试重试一次...")
                try:
                    verify_param = solve_captcha()
                    _, c_res = claim_plan(base_url, api_key, account, plan_id, verify_param)
                    if isinstance(c_res, dict) and c_res.get("ok"):
                        log(f"🎉 重试领取成功: {plan_name}")
                        success_count += 1
                    else:
                        log(f"❌ 重试依然失败: {c_res}")
                except Exception as e:
                    log(f"重试求解验证码异常: {e}")
            else:
                log(f"❌ 领取失败: {c_res.get('message', c_res)}")
        else:
            log(f"❌ 领取响应未知格式: {c_res}")

    # 4. 再次查询最新额度
    log("正在刷新领取后的最新配额状态...")
    time.sleep(1)
    status, updated_quota = query_quota(base_url, api_key, account)
    if status == 200 and isinstance(updated_quota, dict):
        balances = updated_quota.get("balances", [])
        for b in balances:
            if isinstance(b, dict):
                log(f"最新余额 => {b.get('show_name')}: 可用 {b.get('available_units')}/{b.get('total_units')} {b.get('unit_type')}")

    return success_count > 0

if __name__ == "__main__":
    poll_sec = int(sys.argv[1]) if len(sys.argv) > 1 else 10
    interval_sec = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    run_claim_process(poll_seconds=poll_sec, interval=interval_sec)
