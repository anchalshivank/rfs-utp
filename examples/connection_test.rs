use std::io::{stdin, stdout, Write};
use std::process;
use log::{info, Level};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::io::Read;
use std::path::Path;
use chrono::Local;
use colored::Colorize;
use env_logger::Builder;
use utp::utp_stream::UtpStream;

fn usage() -> ! {
    println!("Usage: utp [-s|-c] <address> <port>");
    process::exit(1);
}

fn init_logger() {
    Builder::new()
        .format(|buf, record| {
            // Retrieve the full file path and line number where the log was called
            let file = record.file().unwrap_or("unknown");
            let line = record.line().unwrap_or(0);

            // Define the base directory using the CARGO_MANIFEST_DIR environment variable
            let base_dir = env!("CARGO_MANIFEST_DIR");

            // Create Path objects for the file and base directory
            let file_path = Path::new(file);
            let base_path = Path::new(base_dir);

            // Attempt to create a relative path from the base directory
            let relative_path = file_path.strip_prefix(base_path)
                .map(|path| path.to_str().unwrap_or(file))
                .unwrap_or(file);

            // Apply color to different parts of the log
            let timestamp = Local::now()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
                .yellow(); // Gray

            // Color the log level based on severity
            let level_colored = match record.level() {
                Level::Error => "ERROR".bright_red().bold(),
                Level::Warn  => "WARN".yellow().bold(),
                Level::Info  => "INFO".green().bold(),
                Level::Debug => "DEBUG".cyan(),
                Level::Trace => "TRACE".bright_white(),
            };

            // Color the relative file path
            let file_colored = relative_path.blue();
            let location = format!("{}:{}", file_colored, line);

            // Color the log message
            let message = format!("{}", record.args()).white().bold();

            // Write the formatted and colored log message to the buffer
            writeln!(
                buf,
                "{} [{}] [{}] - {}",
                timestamp,      // Gray
                level_colored,  // Colored based on severity
                location,       // Blue file path with uncolored line number
                message         // White
            )
        })
        .filter_level(log::LevelFilter::Info) // Set the desired log level
        .init();
}

#[tokio::main]
async fn main() {

    init_logger();

    let mut args = std::env::args();
    args.next();

    info!("Running server args: {:?}", args);

    enum Mode {
        Server,
        Client
    }

    let mode: Mode = match args.next() {
        Some(ref s )  if s == "-s" => Mode::Server,
        Some(ref s ) if s == "-c" => Mode::Client,
        _ => usage()
    };

    // Parse the address argument or use a default if none is provided
    let addr = match (args.next(), args.next()) {
        (None, None) => "127.0.0.1:8080".to_owned(),
        (Some(ip), Some(port)) => format!("{}:{}", ip, port),
        _ => usage(),
    };
    let addr: &str = &addr;

    match mode {
        Mode::Server => {
            let mut stream = UtpStream::bind(addr).await.unwrap();
            let mut writer = stdout();
            let _ = writeln!(&mut writer, "Starting server on {}", addr);

            let mut payload = vec![0; 1024 * 1024];
            // loop {
            //     let mut payload = vec![0; 1024];
            //     match stream.socket.recv_from(&mut payload).await {
            //         Ok((len, addr)) => {
            //             log::info!("Received {} bytes from {}: {:?}", len, addr, &payload[..len]);
            //             stdout().write_all(&payload[..len]).unwrap();
            //         }
            //         Err(e) => {
            //             log::error!("Failed to read from socket: {}", e);
            //             break;
            //         }
            //     }
            // }
            //
            //
            loop {
                match stream.read(&mut payload).await {
                    Ok(0) => {
                        info!("Zero-length packet received, continuing...");
                        continue;  // Instead of breaking, just continue
                    },
                    Ok(read) => {
                        info!("Read {} bytes", read);
                        writer.write_all(&payload[..read]).expect("Error writing to stdout");
                        writer.flush().expect("Error flushing stdout");
                    },
                    Err(e) => panic!("Error: {}", e),
                };
            }
        }
        Mode::Client => {
            let mut stream = UtpStream::connect(addr).await.unwrap();
            let mut reader = stdin();

            let mut payload = vec![0; 1024 * 1024];

            loop {
                match reader.read(&mut payload) {
                    Ok(0) => {
                        info!("Connection broke");
                        break
                    },
                    Ok(read) => {
                        // info!("reading :: {:?}", payload);
                        stream.write_all(&payload[..read]).await.expect("Error writing to stream");
                    }
                    Err(e) => {
                        panic!("Error: {}", e)
                    },
                };
            }
        }
    }
}
