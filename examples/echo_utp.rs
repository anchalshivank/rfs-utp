use std::{error::Error, net::SocketAddr, str::FromStr, time::Duration};
use std::path::Path;
use chrono::Local;
use colored::Colorize;
use env_logger::Builder;
use log::Level;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};
use std::io::Write;
use utp::utp_stream::UtpListener;

const UDP_BUFFER_SIZE: usize = 17480; // 17kb
const UDP_TIMEOUT: u64 = 10 * 1000; // 10sec
fn init_logger() {
    Builder::new()
        .format(|buf, record| {
            let file = record.file().unwrap_or("unknown");
            let line = record.line().unwrap_or(0);
            let base_dir = env!("CARGO_MANIFEST_DIR");
            let file_path = Path::new(file);
            let base_path = Path::new(base_dir);
            let relative_path = file_path
                .strip_prefix(base_path)
                .map(|path| path.to_str().unwrap_or(file))
                .unwrap_or(file);

            let timestamp = Local::now()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
                .yellow();

            let level_colored = match record.level() {
                Level::Error => "ERROR".bright_red().bold(),
                Level::Warn => "WARN".yellow().bold(),
                Level::Info => "INFO".green().bold(),
                Level::Debug => "DEBUG".cyan(),
                Level::Trace => "TRACE".bright_white(),
            };

            let file_colored = relative_path.blue();
            let location = format!("{}:{}", file_colored, line);
            let message = format!("{}", record.args()).white().bold();

            writeln!(
                buf,
                "{} [{}] [{}] - {}",
                timestamp,
                level_colored,
                location,
                message
            )
        })
        .filter_level(log::LevelFilter::Info)
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // let level = format!("{}={}", module_path!(), "trace");
    init_logger();

    let listener = UtpListener::bind(SocketAddr::from_str("127.0.0.1:8080")?).await?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let id = std::thread::current().id();
            let block = async move {
                let mut buf = vec![0u8; UDP_BUFFER_SIZE];
                let duration = Duration::from_millis(UDP_TIMEOUT);
                loop {
                    let n = timeout(duration, stream.read(&mut buf)).await??;
                    stream.write_all(&buf[0..n]).await?;
                    log::info!("{:?} echoed {:?} for {} bytes", id, stream.peer_addr(), n);
                }
                #[allow(unreachable_code)]
                Ok::<(), std::io::Error>(())
            };
            if let Err(e) = block.await {
                log::error!("error: {:?}", e);
            }
        });
    }
}
