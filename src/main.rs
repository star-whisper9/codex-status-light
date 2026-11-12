use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use codex_status_light::{
    chatgpt::Source,
    device::{Output, alice75::Alice75},
    state::{StateMachine, Status},
};

struct Options {
    home: PathBuf,
    device: String,
    once: bool,
    seconds: Option<f64>,
}

fn options() -> Result<Option<Options>> {
    let mut args = env::args().skip(1);
    let mut home = None;
    let mut device = "console".to_string();
    let mut once = false;
    let mut seconds = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "codex-status-light [--device console|alice75] [--once] [--seconds N] [--codex-home PATH]\n\n默认在前台输出状态 JSON；--device alice75 才会控制键盘。\n--once 仅观察一次（console）；Ctrl-C 退出，Alice75 返回 wave 二档。\n不会安装后台服务或登录自启。"
                );
                return Ok(None);
            }
            "--device" => device = args.next().context("--device 缺少参数")?,
            "--codex-home" => {
                home = Some(PathBuf::from(args.next().context("--codex-home 缺少路径")?))
            }
            "--once" => once = true,
            "--seconds" => {
                let value: f64 = args
                    .next()
                    .context("--seconds 缺少参数")?
                    .parse()
                    .context("--seconds 必须为数字")?;
                ensure!(value.is_finite() && value > 0.0, "--seconds 必须是有限正数");
                seconds = Some(value);
            }
            _ => bail!("未知参数: {arg}；使用 --help 查看用法"),
        }
    }
    ensure!(
        matches!(device.as_str(), "console" | "alice75"),
        "不支持的设备: {device}"
    );
    ensure!(!once || device == "console", "--once 仅用于 console 观察");
    let home = home
        .or_else(|| env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))
        .context("无法确定 Codex 数据目录，请使用 --codex-home")?;
    Ok(Some(Options {
        home,
        device,
        once,
        seconds,
    }))
}

struct Console;
impl Output for Console {
    fn render(&mut self, _: Status) -> Result<()> {
        Ok(())
    }
    fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

fn run(options: Options) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))
        .context("无法注册退出信号处理器")?;
    let mut output: Box<dyn Output> = match options.device.as_str() {
        "alice75" => Box::new(Alice75::open()?),
        _ => Box::new(Console),
    };
    let (tx, rx) = mpsc::channel();
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        let mut source = Source::new(options.home);
        while !worker_stop.load(Ordering::Relaxed) {
            let snapshot = source.snapshot();
            let failed = snapshot.is_err();
            if tx.send(snapshot).is_err() || failed {
                break;
            }
            // 保持一秒轮询；退出时由主线程唤醒，避免每 100ms 唤醒检查停止标志。
            let next_poll = Instant::now() + Duration::from_secs(1);
            while !worker_stop.load(Ordering::Relaxed) && Instant::now() < next_poll {
                thread::park_timeout(next_poll.saturating_duration_since(Instant::now()));
            }
        }
    });
    let started = Instant::now();
    let mut machine = StateMachine::default();
    let mut previous = None;
    let mut observed = false;
    let result: Result<()> = (|| {
        while !stop.load(Ordering::Relaxed) {
            if options
                .seconds
                .is_some_and(|limit| started.elapsed().as_secs_f64() >= limit)
            {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(snapshot) => {
                    machine
                        .observe(snapshot.context("ChatGPT 状态读取失败")?, started.elapsed())?;
                    observed = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("状态读取线程意外退出"),
            }
            if !observed {
                continue;
            }
            let status = machine.status(started.elapsed());
            if previous != Some(status) {
                output.render(status).context("设备输出失败")?;
                writeln!(io::stdout().lock(), "{}", serde_json::to_string(&status)?)
                    .context("写入状态输出失败")?;
                previous = Some(status);
            }
            if options.once {
                break;
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    worker.thread().unpark();
    drop(rx);
    let joined = worker
        .join()
        .map_err(|_| anyhow::anyhow!("状态读取线程 panic"));
    let shutdown = output.shutdown().context("设备退出处理失败");
    // 退出清理失败不覆盖最初错误，也不静默丢弃。
    if let Err(error) = &joined {
        eprintln!("{error:#}");
    }
    if let Err(error) = &shutdown {
        eprintln!("{error:#}");
    }
    result.and(joined).and(shutdown)
}

fn main() -> ExitCode {
    let result = options().and_then(|options| match options {
        Some(options) => run(options),
        None => Ok(()),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
