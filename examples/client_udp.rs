use std::{error::Error, net::SocketAddr, str::FromStr};
use std::path::Path;
use chrono::Local;
use colored::Colorize;
use env_logger::Builder;
use log::Level;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use utp::udp_stream::UdpStream;
use std::io::Write;
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
    let level = format!("{}={}", module_path!(), "trace");
    init_logger();
    let mut stream = UdpStream::connect(SocketAddr::from_str("127.0.0.1:8080")?).await?;
    log::info!("Ready to Connected to {}", &stream.peer_addr()?);
    let mut buffer = String::new();
    loop {
        std::io::stdin().read_line(&mut buffer)?;
        stream.write_all(buffer.as_bytes()).await?;
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await?;
        log::info!("-> {}", String::from_utf8_lossy(&buf[..n]));
        buffer.clear();
    }
}
