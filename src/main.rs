#[macro_use]
extern crate tracing;

use monoio::{
    io::{AsyncWriteRent, AsyncWriteRentExt},
    net::TcpStream,
};
use rand::seq::SliceRandom;
use rkyv::vec::ArchivedVec;
use std::{
    fs::File,
    mem,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    str,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WHERE_TO: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(151, 217, 2, 166)), 1337);

macro_rules! attempt {
    ($io:expr) => {{
        let (result, buf) = { $io };
        result?;
        buf
    }};
}

#[inline]
fn encode_dec(
    num: impl lexical::ToLexicalWithOptions<Options = lexical::WriteIntegerOptions>,
) -> Vec<u8> {
    static DEC_FMT: u128 = lexical::NumberFormatBuilder::decimal();
    static OPTS: lexical::WriteIntegerOptions = lexical::WriteIntegerOptions::new();

    lexical::to_string_with_options::<_, DEC_FMT>(num, &OPTS).into_bytes()
}

async fn release_the_kraken(
    conn: &mut TcpStream,
    frame: &riptide_common::ArchivedFrame,
    (x_offset, y_offset): (usize, usize),
) -> anyhow::Result<()> {
    info!("sending frame..");

    // choose random y-lane order
    let mut y_idxs: Vec<usize> = (0..frame.data.len()).collect();
    y_idxs.shuffle(&mut rand::thread_rng());

    for y_pos in y_idxs {
        let y_lane = &frame.data[y_pos];
        for (x_pos, pixel) in y_lane.iter().enumerate() {
            attempt!(conn.write_all(b"PX ").await);

            let x_str = encode_dec(x_pos + x_offset);
            attempt!(conn.write_all(x_str).await);
            attempt!(conn.write_all(b" ").await);

            let y_str = encode_dec(y_pos + y_offset);
            attempt!(conn.write_all(y_str).await);
            attempt!(conn.write_all(b" ").await);

            // encode pixel as hex
            attempt!(conn.write_all(pixel.hex_repr.to_vec()).await);
            attempt!(conn.write_all(b"\n").await);
        }
    }

    conn.flush().await?;

    Ok(())
}

async fn connect(addr: SocketAddr) -> anyhow::Result<TcpStream> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn build_conn_pool(addr: SocketAddr, num_conn: usize) -> anyhow::Result<Vec<TcpStream>> {
    let mut conn = Vec::new();
    for idx in 0..num_conn {
        info!("building conn {idx}");
        conn.push(connect(addr).await?);
    }

    Ok(conn)
}

#[derive(Clone, argh::FromArgs)]
/// Pixelflut client to play cool videos :P
struct Args {
    #[argh(option)]
    /// framerate of the video
    framerate: f32,

    #[argh(option, default = "50")]
    /// amount of parallel connections open to flood
    num_conn: usize,

    #[argh(option, default = "WHERE_TO")]
    /// address of the pixelflut server
    addr: SocketAddr,

    #[argh(option)]
    /// file containing the frame data
    data: PathBuf,

    #[argh(option, default = "0")]
    /// x offset
    x_offset: usize,

    #[argh(option, default = "0")]
    /// y offset
    y_offset: usize,

    #[argh(option)]
    /// start at the provided unix timestamp
    start_at: Option<u64>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args: Args = argh::from_env();

    info!("loading data..");
    let data_file = File::open(&args.data)?;
    let data = unsafe { memmap2::Mmap::map(&data_file)? };
    let frames: &ArchivedVec<riptide_common::ArchivedFrame> =
        rkyv::access::<_, rkyv::rancor::Error>(&data).unwrap();
    let frames = unsafe {
        mem::transmute::<&[riptide_common::ArchivedFrame], &'static [riptide_common::ArchivedFrame]>(
            frames,
        )
    };

    info!("loaded data successfully");

    if let Some(at_timestamp) = args.start_at {
        info!("waiting until {at_timestamp}..");
        let point_in_time = SystemTime::UNIX_EPOCH + Duration::from_secs(at_timestamp);
        let duration = point_in_time.duration_since(SystemTime::now())?;

        thread::sleep(duration);
    }

    let sleep_duration = Duration::from_secs_f32(1.0 / args.framerate);
    let mut frame_ctr = 0;
    let current_frame = Arc::new(AtomicUsize::new(&frames[frame_ctr] as *const _ as usize));

    info!("starting riptide >:3");

    for idx in 0..thread::available_parallelism().unwrap().into() {
        info!("spawning runtime {idx}");

        let current_frame = current_frame.clone();
        thread::spawn({
            let args = args.clone();

            move || {
                let mut runtime = monoio::RuntimeBuilder::<
                    monoio::time::TimeDriver<monoio::IoUringDriver>,
                >::new()
                .build()
                .unwrap();

                runtime
                    .block_on(async move {
                        info!("building conn pool");
                        let pool = build_conn_pool(args.addr, args.num_conn).await?;

                        info!("spawning streams");
                        for mut stream in pool {
                            let current_frame = Arc::clone(&current_frame);

                            monoio::time::sleep(Duration::from_millis(2)).await;

                            monoio::spawn(async move {
                                loop {
                                    let frame = current_frame.load(Ordering::Relaxed);
                                    let frame: &riptide_common::ArchivedFrame =
                                        unsafe { &*(frame as *const _) };

                                    if let Err(error) = release_the_kraken(
                                        &mut stream,
                                        frame,
                                        (args.x_offset, args.y_offset),
                                    )
                                    .await
                                    {
                                        error!(?error, "sending failed :((");
                                        stream = connect(args.addr).await.unwrap();
                                    }
                                }
                            });
                        }

                        std::future::pending::<anyhow::Result<()>>().await
                    })
                    .unwrap();
            }
        });
    }

    loop {
        frame_ctr += 1;
        frame_ctr %= frames.len();

        thread::sleep(sleep_duration);

        info!("switching to frame {frame_ctr}");
        current_frame.store(&frames[frame_ctr] as *const _ as usize, Ordering::Relaxed);
    }
}
