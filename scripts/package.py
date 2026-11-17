#!/usr/bin/env python3
"""将已构建的本机 release 二进制与服务管理入口打成独立目录和 ZIP。"""
import argparse
from pathlib import Path
import platform
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT/'target/package')
    args = parser.parse_args()
    binary = ROOT/'target/release/codex-status-light'
    if not binary.is_file():
        raise RuntimeError('请先执行 cargo build --release --locked')
    # 输出目录存在时明确失败，防止误覆盖正在使用的服务文件。
    bundle = args.output.resolve()/'CodexStatusLight'
    bundle.mkdir(parents=True, exist_ok=False)
    shutil.copy2(binary, bundle/'codex-status-light')
    shutil.copy2(ROOT/'scripts/service.py', bundle/'service.py')
    for name, action in [('Start', 'start'), ('StartHalo96', 'start --device halo96'), ('Stop', 'stop'), ('Status', 'status')]:
        script = bundle/(name+'.command')
        script.write_text('#!/bin/zsh\nset -eu\ncd -- "$(dirname -- "$0")"\nexec /usr/bin/python3 service.py '+action+'\n')
        script.chmod(0o755)
    (bundle/'README.txt').write_text('Codex Status Light\n\nStart.command：启动 Alice75 灯效服务并启用登录自启。\nStartHalo96.command：启动 USB Halo96 v2 背光服务；切换设备前先停止现有服务。\nStop.command：停止服务并取消登录自启，Alice75 返回 wave 二档，Halo96 恢复接管前背光。\nStatus.command：查看 launchd 状态和最近日志。\n\n保持本目录位置不变；迁移或更新前先停止服务。\n服务由 Rust 二进制直接运行，Python 只用于手动管理。\n日志：~/Library/Logs/CodexStatusLight/\n配置：~/Library/LaunchAgents/local.codex-status-light.plist\n键盘断开或状态源报错时，launchd 每隔至少 30 秒重新尝试启动。\n')
    subprocess.run(['/usr/bin/ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(bundle),
                    str(args.output.resolve()/f'CodexStatusLight-macos-{platform.machine()}.zip')], check=True)
    print(bundle)


if __name__ == '__main__':
    main()
