//! 控制管道 `\\.\pipe\wtfs`（spec §5.3）：一行 JSON 请求 / 一行 JSON 响应。
//!
//! * 服务端：`CreateNamedPipeW`（字节模式、拒绝远程）+ 阻塞
//!   `ConnectNamedPipe`；连接成功后把 HANDLE 包成 [`std::fs::File`] 交给
//!   线程，用 std 读写——Win32 面缩到最小。停机经 `CancelIoEx` 唤醒阻塞的
//!   accept，监听线程随 stop 标志退出。
//! * 客户端：纯 std（`OpenOptions` 打开管道名即 `CreateFileW` 语义），
//!   `ERROR_PIPE_BUSY`/实例空窗自动重试直到超时。
//! * 协议：请求见 [`PipeRequest`]；响应一律 `{"ok":true, ...}` /
//!   `{"ok":false,"error":"..."}`（[`ok_value`]/[`err_value`]）。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, HANDLE, ERROR_PIPE_CONNECTED};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

/// 请求行上限（防异常客户端撑爆内存）。
const MAX_LINE_BYTES: usize = 1 << 20;

/// 控制请求。`Gc` 的 `max_age_days` 覆盖 daemon 默认旧模板阈值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum PipeRequest {
    Status,
    /// 注册一个 CAS 视图（`wtfs worktree-add` 的挂载侧）。
    RegisterCas {
        name: String,
        real_worktree: String,
        head: String,
        #[serde(default)]
        absorbing: bool,
    },
    /// 结束 checkout 吸收窗口（`worktree-add` 在 reset --hard 后调用）。
    FinishCas { name: String },
    /// 注销 CAS 视图（`worktree-remove` 删除前调用）。
    RemoveCas { name: String },
    Gc {
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        max_age_days: Option<u64>,
    },
    Stop,
}

/// 响应构造：`{"ok":true, <data>}`。
pub fn ok_value(data: serde_json::Value) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("ok".into(), serde_json::Value::Bool(true));
    match data {
        serde_json::Value::Object(object) => {
            for (k, v) in object {
                map.insert(k, v);
            }
        }
        serde_json::Value::Null => {}
        other => {
            map.insert("data".into(), other);
        }
    }
    serde_json::Value::Object(map)
}

/// 响应构造：`{"ok":false,"error":...}`。
pub fn err_value(message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": message.into() })
}

/// 请求处理器：返回响应 JSON。
pub type Handler = Arc<dyn Fn(&PipeRequest) -> serde_json::Value + Send + Sync>;

/// 管道服务控制句柄：停机 + 唤醒阻塞的 accept。
#[derive(Clone)]
pub struct PipeControl {
    stop: Arc<AtomicBool>,
    /// 处于 `ConnectNamedPipe` 等待中的 HANDLE 值（CancelIoEx 目标）。
    pending: Arc<Mutex<Vec<usize>>>,
}

impl PipeControl {
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 请求停止：置标志并取消在途 accept。
    ///
    /// 句柄的移除由监听线程在 `ConnectNamedPipe` 返回后负责（见
    /// [`PipeServer::spawn`] 内的 listen_loop），这里只取消、不清空 pending，
    /// 并短暂重试数次——覆盖“cancel 先于 IO 发起”的竞态窗口（stop 时句柄
    /// 已在 pending、但监听线程尚未调 ConnectNamedPipe：首轮 CancelIoEx 是
    /// no-op，重试轮会在 IO 真正挂起后命中）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for _ in 0..5 {
            let handles = self.pending.lock().clone();
            if handles.is_empty() {
                break;
            }
            for raw in &handles {
                unsafe {
                    let _ = CancelIoEx(HANDLE(*raw as *mut _), None);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

pub struct PipeServer;

impl PipeServer {
    /// 启动监听线程。返回（控制句柄, 监听线程）；线程在 stop 后退出，
    /// daemon 关停路径 join 它以回收 handler 持有的 Arc。
    pub fn spawn(name: String, handler: Handler) -> std::io::Result<(PipeControl, std::thread::JoinHandle<()>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(Mutex::new(Vec::new()));
        let control = PipeControl {
            stop: stop.clone(),
            pending: pending.clone(),
        };
        let join = std::thread::Builder::new()
            .name("wtfs-pipe-listen".to_string())
            .spawn(move || listen_loop(name, handler, stop, pending))?;
        Ok((control, join))
    }
}

fn listen_loop(
    name: String,
    handler: Handler,
    stop: Arc<AtomicBool>,
    pending: Arc<Mutex<Vec<usize>>>,
) {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut first_instance = true;
    tracing::info!(pipe = %name, "control pipe listening");

    while !stop.load(Ordering::Relaxed) {
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first_instance {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe_mode: NAMED_PIPE_MODE =
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                open_mode,
                pipe_mode,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            let code = unsafe { GetLastError() };
            tracing::warn!(pipe = %name, code = code.0, "CreateNamedPipeW failed");
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        first_instance = false;

        let raw = handle.0 as usize;
        pending.lock().push(raw);
        if stop.load(Ordering::Relaxed) {
            pending.lock().retain(|h| *h != raw);
            // 停机：释放刚创建的实例（CloseHandle），监听线程退出。
            drop(unsafe { OwnedHandle::from_raw_handle(handle.0) });
            break;
        }

        // 阻塞 accept。句柄在 pending 中保留到 ConnectNamedPipe 返回之后才移除，
        // 保证此窗口内的 stop() 仍能 CancelIoEx 命中（评审 P2-2）。
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        pending.lock().retain(|h| *h != raw);

        let client_ready = match &connected {
            Ok(()) => true,
            Err(e) => win32_code(e) == ERROR_PIPE_CONNECTED.0,
        };
        if !client_ready {
            // 未连接（含 stop 触发的 ERROR_OPERATION_ABORTED）：立即释放实例，
            // 否则残留的监听实例会让停机后的客户端连上并永久阻塞。
            let stopped = stop.load(Ordering::Relaxed);
            drop(unsafe { OwnedHandle::from_raw_handle(handle.0) });
            if stopped {
                break;
            }
            let code = match &connected {
                Err(e) => Some(win32_code(e)),
                Ok(()) => None,
            };
            tracing::warn!(pipe = %name, code = ?code, "ConnectNamedPipe failed");
            continue;
        }

        // 交给连接线程；OwnedHandle 负责 CloseHandle。
        let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        let thread_handler = handler.clone();
        let spawned = std::thread::Builder::new()
            .name("wtfs-pipe-conn".to_string())
            .spawn(move || {
                let file = File::from(owned);
                serve_connection(file, &thread_handler);
            });
        if spawned.is_err() {
            tracing::error!("failed to spawn pipe connection thread");
        }
    }
    tracing::info!(pipe = %name, "control pipe listener stopped");
}

/// 单连接服务：一行请求 → 一行响应；客户端断开即结束。
fn serve_connection(file: File, handler: &Handler) {
    let mut reader = BufReader::new(match file.try_clone() {
        Ok(clone) => clone,
        Err(e) => {
            tracing::warn!(error = %e, "pipe try_clone failed");
            return;
        }
    });
    // 读循环内限长（io::Take 对 BufRead 透传 read_until），无换行的异常
    // 客户端最多消耗 MAX_LINE_BYTES+1 字节，超出即断开（评审 P3）。
    let mut bytes = Vec::new();
    let read = {
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);
        limited.read_until(b'\n', &mut bytes)
    };
    match read {
        Ok(0) => return, // 客户端未发请求即断开。
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "pipe read failed");
            return;
        }
    }
    if !bytes.ends_with(b"\n") || bytes.len() > MAX_LINE_BYTES {
        tracing::warn!(len = bytes.len(), "pipe request too large; dropping connection");
        return;
    }
    let line = String::from_utf8_lossy(&bytes);
    let response = match serde_json::from_str::<PipeRequest>(line.trim()) {
        Ok(request) => handler(&request),
        Err(e) => err_value(format!("invalid request: {e}")),
    };
    let mut file = file;
    if let Err(e) = file.write_all(response.to_string().as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
    {
        tracing::warn!(error = %e, "pipe write failed");
    }
}

/// 客户端：发一条请求、收一条响应；管道忙/实例空窗自动重试。
pub fn call(
    name: &str,
    request: &PipeRequest,
    timeout: Duration,
) -> anyhow::Result<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    let payload = format!("{}\n", serde_json::to_string(request)?);
    loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(mut file) => {
                file.write_all(payload.as_bytes())?;
                file.flush()?;
                let mut reader = BufReader::new(file);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let value: serde_json::Value = serde_json::from_str(line.trim())
                    .map_err(|e| anyhow::anyhow!("invalid pipe response {line:?}: {e}"))?;
                return Ok(value);
            }
            Err(e) => {
                let busy = e.raw_os_error() == Some(ERROR_PIPE_BUSY_RAW);
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!("pipe {name} unreachable: {e}"));
                }
                std::thread::sleep(if busy {
                    Duration::from_millis(20)
                } else {
                    Duration::from_millis(100)
                });
            }
        }
    }
}

/// `ERROR_PIPE_BUSY` 的 raw OS error code（std 不暴露常量）。
const ERROR_PIPE_BUSY_RAW: i32 = 231;

/// `windows::core::Error` 的 HRESULT → Win32 错误码（非 win32 设施返回 0）。
fn win32_code(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn test_pipe_name(tag: &str) -> String {
        format!(r"\\.\pipe\wtfs-test-{tag}-{}", std::process::id())
    }

    fn spawn_echo(name: String) -> PipeControl {
        let handler: Handler = Arc::new(|req| match req {
            PipeRequest::Status => ok_value(serde_json::json!({"status": {"pid": 1}})),
            PipeRequest::Stop => ok_value(serde_json::json!({})),
            PipeRequest::Gc { dry_run, .. } => {
                ok_value(serde_json::json!({"gc": {"dry_run": dry_run}}))
            }
            PipeRequest::RegisterCas { .. }
            | PipeRequest::FinishCas { .. }
            | PipeRequest::RemoveCas { .. } => ok_value(serde_json::json!({})),
        });
        PipeServer::spawn(name, handler).expect("spawn server").0
    }

    #[test]
    fn round_trip_and_concurrent_clients() {
        let name = test_pipe_name("rt");
        let control = spawn_echo(name.clone());

        let response = call(&name, &PipeRequest::Status, Duration::from_secs(5)).unwrap();
        assert_eq!(response["ok"], serde_json::Value::Bool(true));
        assert_eq!(response["status"]["pid"], 1);

        let response =
            call(&name, &PipeRequest::Gc { dry_run: true, max_age_days: Some(3) }, Duration::from_secs(5))
                .unwrap();
        assert_eq!(response["gc"]["dry_run"], serde_json::Value::Bool(true));

        // 并发连接（每连接一线程、实例自动补充）。
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let name = name.clone();
                thread::spawn(move || {
                    call(&name, &PipeRequest::Status, Duration::from_secs(5)).unwrap()
                })
            })
            .collect();
        for handle in handles {
            let response = handle.join().unwrap();
            assert_eq!(response["status"]["pid"], 1);
        }

        // 无效请求 → 错误响应（连接不被打断）。
        let mut file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
        file.write_all(b"garbage\n").unwrap();
        file.flush().unwrap();
        let mut line = String::new();
        BufReader::new(file).read_line(&mut line).unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["ok"], serde_json::Value::Bool(false));

        control.stop();
    }

    #[test]
    fn unreachable_pipe_times_out() {
        let name = test_pipe_name("missing");
        let started = Instant::now();
        let err = call(&name, &PipeRequest::Status, Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(err.to_string().contains("unreachable"));
    }

    #[test]
    fn stop_terminates_listener() {
        let name = test_pipe_name("stop");
        let handler: Handler = Arc::new(|_| ok_value(serde_json::json!({})));
        let (control, join) = PipeServer::spawn(name.clone(), handler).unwrap();
        // 先来一回成功请求，让监听循环推进过一轮。
        let response = call(&name, &PipeRequest::Status, Duration::from_secs(5)).unwrap();
        assert_eq!(response["ok"], serde_json::Value::Bool(true));
        control.stop();
        join_with_watchdog(join, "listener exits on stop");
        // 停机后客户端不再可达（不再有监听实例）。
        assert!(call(&name, &PipeRequest::Status, Duration::from_millis(300)).is_err());
    }

    /// P2-2 回归：在监听线程 push 句柄与发起 ConnectNamedPipe 之间的竞态
    /// 窗口内 stop，监听线程必须仍能退出（CancelIoEx 必须命中）。窗口是
    /// 微秒级、无法确定性复现，用多轮“spawn 即 stop + 有请求穿插”的压力
    /// 循环逼近；任一轮 join 超时即失败。
    #[test]
    fn stop_race_stress_never_hangs() {
        for round in 0..30 {
            let name = test_pipe_name("race");
            let handler: Handler = Arc::new(|req| match req {
                PipeRequest::Status => ok_value(serde_json::json!({"ok echo": true})),
                _ => ok_value(serde_json::json!({})),
            });
            let (control, join) = PipeServer::spawn(name.clone(), handler).unwrap();
            // 穿插请求：把监听循环推进过 accept/移交各阶段。超时取短值——
            // stop 抢先取消 accept 时调用方按预期失败，不能拖慢整个测试。
            let caller = {
                let name = name.clone();
                std::thread::spawn(move || {
                    let _ = call(&name, &PipeRequest::Status, Duration::from_millis(300));
                })
            };
            control.stop();
            caller.join().expect("caller thread");
            join_with_watchdog(join, &format!("round {round}: listener must exit on stop"));
        }
    }

    /// join 的看门狗：超时 panic（否则挂死会拖死整个测试进程）。
    fn join_with_watchdog(join: std::thread::JoinHandle<()>, what: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = join.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("timeout waiting for {what}"));
    }

    #[test]
    fn oversized_request_is_capped_and_dropped() {
        let name = test_pipe_name("cap");
        let handler: Handler = Arc::new(|_| ok_value(serde_json::json!({"served": true})));
        let (control, _join) = PipeServer::spawn(name.clone(), handler).unwrap();

        // 无换行且超过 MAX_LINE_BYTES：服务端在读取循环内截断并断开，
        // 不响应、不无界缓冲。
        let mut file = open_pipe_with_retry(&name, Duration::from_secs(5));
        let junk = vec![b'a'; MAX_LINE_BYTES + 4096];
        file.write_all(&junk).unwrap();
        file.flush().unwrap();
        let mut line = String::new();
        // 服务端不回响应且直接断开：读到 EOF（0 字节）。
        let n = BufReader::new(file).read_line(&mut line).unwrap();
        assert_eq!(n, 0, "server must drop oversized connection, got {line:?}");

        // 管道实例随后被服务端释放；正常请求仍可用（新实例）。
        let response = call(&name, &PipeRequest::Status, Duration::from_secs(5)).unwrap();
        assert_eq!(response["served"], serde_json::Value::Bool(true));
        control.stop();
    }

    /// 与服务端 `call` 同语义的带重试打开（监听线程建实例有间隙）。
    fn open_pipe_with_retry(name: &str, timeout: Duration) -> std::fs::File {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match OpenOptions::new().read(true).write(true).open(name) {
                Ok(file) => return file,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("pipe {name} never became available: {e}"),
            }
        }
    }
}
