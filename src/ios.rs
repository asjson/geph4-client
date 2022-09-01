use std::{
    ffi::{CStr, CString},
    format,
    io::{BufRead, BufReader, Write},
    net::{IpAddr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    os::raw::{c_char, c_int, c_uchar},
    time::Duration,
};

use bytes::Bytes;
use geph4_protocol::EndpointSource;
use once_cell::sync::Lazy;
use os_pipe::PipeReader;
use parking_lot::Mutex;
use structopt::StructOpt;

use crate::{
    logs::LogBuffer,
    main_binderproxy, main_bridgetest, main_connect, main_sync,
    vpn::{DOWN_CHANNEL, UP_CHANNEL},
    Opt,
};

pub static LOG_BUFFER: Lazy<Mutex<LogBuffer>> = Lazy::new(|| Mutex::new(LogBuffer::new(200000)));

pub static LOG_LINES: Lazy<Mutex<BufReader<PipeReader>>> = Lazy::new(|| {
    let (read, write) = os_pipe::pipe().unwrap();
    let write = Mutex::new(write);
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("geph4client=debug,geph4_protocol=debug,warn"),
    )
    .format_timestamp_millis()
    // .target(env_logger::Target::Pipe(Box::new(std::io::sink())))
    .format(move |buf, record| {
        let line = format!(
            "[{} {}]: {}",
            record.level(),
            record.module_path().unwrap_or("none"),
            record.args()
        );
        LOG_BUFFER.lock().add_line(&line);

        let mut write = write.lock();
        writeln!(buf, "{}", line).unwrap();
        writeln!(write, "{}", line)
    })
    .init();

    Mutex::new(BufReader::new(read))
});

fn config_logging_ios() {
    log::debug!("TRYING TO CONFIG LOGGING HERE");
    Lazy::force(&LOG_LINES);
}

fn dispatch_ios(opt: Opt) -> anyhow::Result<String> {
    config_logging_ios();
    let version = env!("CARGO_PKG_VERSION");
    log::info!("IOS geph4-client v{} starting...", version);

    smolscale::block_on(async move {
        match opt {
            Opt::Connect(opt) => loop {
                if let Err(err) = main_connect::main_connect(opt.clone()).await {
                    log::error!("Something SERIOUSLY wrong has happened! {:#?}", err);
                    smol::Timer::after(Duration::from_secs(1)).await;
                };
            },
            Opt::Sync(opt) => main_sync::sync_json(opt).await,
            Opt::BinderProxy(opt) => {
                main_binderproxy::main_binderproxy(opt).await?;
                Ok(String::from(""))
            }
            Opt::BridgeTest(opt) => {
                main_bridgetest::main_bridgetest(opt).await?;
                Ok(String::from(""))
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn call_geph(opt: *const c_char) -> *mut c_char {
    let inner = || {
        let c_str = unsafe { CStr::from_ptr(opt) };
        // if c_str.to_str()?.contains("connect") {
        //     anyhow::bail!("lol always fail connects")
        // }
        let args: Vec<&str> = serde_json::from_str(c_str.to_str()?)?;
        std::env::set_var("GEPH_RECURSIVE", "1"); // no forking in iOS
        let opt: Opt = Opt::from_iter_safe(args)?;
        dispatch_ios(opt)
    };

    let output = match inner() {
        Ok(output) => output,
        Err(err) => format!("ERROR!!!! {:?}", err),
    };

    CString::new(output).unwrap().into_raw()
}

#[no_mangle]
pub extern "C" fn upload_packet(pkt: *const c_uchar, len: c_int) {
    unsafe {
        let slice = std::slice::from_raw_parts(pkt as *mut u8, len as usize);
        let owned = slice.to_vec();
        let bytes: Bytes = owned.into();
        UP_CHANNEL.0.send(bytes).unwrap();
    }
}

#[no_mangle]
pub extern "C" fn download_packet(buffer: *mut c_uchar, buflen: c_int) -> c_int {
    let pkt = DOWN_CHANNEL.1.recv().unwrap();
    // let pkt = "111111".as_bytes();
    // log::debug!("from geph: downloaded packet!");
    let pkt_ref = pkt.as_ref();
    unsafe {
        let mut slice: &mut [u8] =
            std::slice::from_raw_parts_mut(buffer as *mut u8, buflen as usize);
        if pkt.len() < slice.len() {
            if slice.write_all(pkt_ref).is_err() {
                log::debug!("from geph: error writing to buffer!");
                -1
            } else {
                pkt.len() as c_int
            }
        } else {
            log::debug!("from geph: buffer too small!");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn check_bridges(buffer: *mut c_char, buflen: c_int) -> c_int {
    let mut whitelist: Vec<IpAddr> = Vec::new();
    if let Some(tun) = main_connect::TUNNEL.read().clone() {
        let endpoint = tun.get_endpoint();
        match endpoint {
            EndpointSource::Independent { endpoint: _ } => {
                -1 // independent exits not supported for iOS
            }
            EndpointSource::Binder(binder_tunnel_params) => {
                let cached_binder = binder_tunnel_params.ccache;
                let exits = smol::block_on(cached_binder.get_exits()).unwrap();
                for exit in exits {
                    if let Ok(server_addr) = smol::block_on(
                        geph4_protocol::getsess::ipv4_addr_from_hostname(exit.hostname.clone()),
                    ) {
                        whitelist.push(server_addr.ip());
                        // bridges
                        if let Ok(bridges) =
                            smol::block_on(cached_binder.get_bridges(&exit.hostname, true))
                        {
                            for bridge in bridges {
                                let ip = bridge.endpoint.ip();
                                whitelist.push(ip);
                            }
                        }
                    }
                }
                let whitelist = serde_json::json!(whitelist).to_string();
                log::debug!(
                    "whitelist is {}; with length {}",
                    whitelist,
                    whitelist.len()
                );

                unsafe {
                    let mut slice =
                        std::slice::from_raw_parts_mut(buffer as *mut u8, buflen as usize);
                    if whitelist.len() < slice.len() {
                        if slice.write_all(whitelist.as_bytes()).is_err() {
                            log::debug!("check bridges failed: writing to buffer failed");
                            -1
                        } else {
                            whitelist.len() as c_int
                        }
                    } else {
                        log::debug!("check bridges failed: buffer not big enough");
                        -1
                    }
                }
            }
        }
    } else {
        log::debug!("check bridges failed: no tunnel");
        -1
    }
}

#[no_mangle]
// returns one line of logs
pub extern "C" fn get_logs(buffer: *mut c_char, buflen: c_int) -> c_int {
    let mut line = String::new();
    if LOG_LINES.lock().read_line(&mut line).is_err() {
        return -1;
    }

    unsafe {
        let mut slice: &mut [u8] =
            std::slice::from_raw_parts_mut(buffer as *mut u8, buflen as usize);
        if line.len() < slice.len() {
            if slice.write_all(line.as_bytes()).is_err() {
                -1
            } else {
                line.len() as c_int
            }
        } else {
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn test_listening_ports() {
    log::debug!("entered test_listening_ports");

    let all_interfaces = SocketAddr::V4(SocketAddrV4::new(
        std::net::Ipv4Addr::new(0, 0, 0, 0),
        10010,
    ));

    let listener = TcpListener::bind(all_interfaces).unwrap();

    log::debug!(
        "the listener looks like it's bound to {:?} ({:?})",
        listener.local_addr().unwrap(),
        listener
    );

    std::thread::spawn(move || {
        // accept connections and process them serially
        for stream in listener.incoming() {
            let _ = stream.unwrap().write(&[1]);
            log::debug!("accepted a TCP connection on 0.0.0.0:10010!");
        }
    });
    match TcpStream::connect("127.0.0.1:10010") {
        Ok(_) => log::debug!("Connected to the server listening on 0.0.0.0!"),
        Err(e) => log::debug!("{}", e),
    }
}
