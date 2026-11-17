#!/usr/bin/env python3
"""管理当前目录内 Rust 二进制的用户级 launchd 服务。"""
import argparse
import os
from pathlib import Path
import plistlib
import subprocess
import time

LABEL = 'local.codex-status-light'
ROOT = Path(__file__).resolve().parent
BINARY = ROOT / 'codex-status-light'
PLIST = Path.home() / 'Library/LaunchAgents' / (LABEL + '.plist')
LOGS = Path.home() / 'Library/Logs/CodexStatusLight'
TARGET = f'gui/{os.getuid()}/{LABEL}'


def status():
    return subprocess.run(['/bin/launchctl', 'print', TARGET], capture_output=True, text=True)


def start(device="alice75"):
    if not BINARY.is_file():
        raise RuntimeError(f'找不到服务二进制：{BINARY}')
    if status().returncode == 0:
        raise RuntimeError('服务已经加载；更新程序前请先停止服务')
    LOGS.mkdir(parents=True, exist_ok=True)
    PLIST.parent.mkdir(parents=True, exist_ok=True)
    config = {
        'Label': LABEL,
        'ProgramArguments': [str(BINARY), '--device', device, '--codex-home', str(Path.home()/'.codex')],
        'WorkingDirectory': str(ROOT),
        'EnvironmentVariables': {'HOME': str(Path.home())},
        'RunAtLoad': True,
        'KeepAlive': True,
        'ThrottleInterval': 30,
        'ExitTimeOut': 10,
        'ProcessType': 'Background',
        'StandardOutPath': str(LOGS/'state.log'),
        'StandardErrorPath': str(LOGS/'error.log'),
    }
    PLIST.write_bytes(plistlib.dumps(config))
    PLIST.chmod(0o644)
    subprocess.run(['/bin/launchctl', 'bootstrap', f'gui/{os.getuid()}', str(PLIST)], check=True)
    print('服务已加载，登录后自动运行。请用 Status.command 查看运行状态。')


def stop():
    if status().returncode == 0:
        subprocess.run(['/bin/launchctl', 'bootout', TARGET], check=True)
        for _ in range(30):
            if status().returncode != 0:
                break
            time.sleep(.5)
        else:
            raise RuntimeError('服务尚未退出，请检查 launchctl 状态')
    if PLIST.exists():
        PLIST.unlink()
    print('服务已停止，登录自启已移除。')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['start', 'stop', 'status'])
    parser.add_argument('--device', choices=['alice75', 'halo96'], default='alice75', help='启动时使用的设备')
    args = parser.parse_args()
    action = args.command
    if action == 'start':
        start(args.device)
    elif action == 'stop':
        stop()
    else:
        result = status()
        print(result.stdout if result.returncode == 0 else '服务未加载。')
        for name in ('state.log', 'error.log'):
            path = LOGS/name
            if path.exists():
                # 只读取末尾，避免日志积累后把整个文件载入内存。
                with path.open('rb') as stream:
                    stream.seek(max(0, path.stat().st_size - 8192))
                    lines = stream.read().decode('utf-8', errors='replace').splitlines()
                print(f'{name}:\n' + '\n'.join(lines[-8:]))


if __name__ == '__main__':
    main()
