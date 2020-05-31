use crossbeam_channel::unbounded;
use profiler_symbol_server::start_server;
use serde_json::to_writer;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use structopt::StructOpt;
use which::which;

mod dyld_bindings;
mod gecko_profile;
mod proc_maps;
mod process_launcher;
mod sampler;
mod task_profiler;
mod thread_profiler;

pub mod kernel_error;
pub mod thread_act;
pub mod thread_info;

use process_launcher::{MachError, ProcessLauncher};
use sampler::Sampler;
use task_profiler::TaskProfiler;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "perfrecord",
    about = r#"Run a command and record a CPU profile of its execution.

EXAMPLES:
    perfrecord ./yourcommand args
    perfrecord --launch-when-done ./yourcommand args
    perfrecord -o prof.json ./yourcommand args
    perfrecord --launch prof.json"#
)]
struct Opt {
    /// Launch the profiler after recording and display the collected profile.
    #[structopt(long = "launch-when-done")]
    launch_when_done: bool,

    /// Sampling interval, in seconds
    #[structopt(short = "i", long = "interval", default_value = "0.001")]
    interval: f64,

    /// Limit the recorded time to the specified number of seconds
    #[structopt(short = "t", long = "time-limit")]
    time_limit: Option<f64>,

    /// Save the collected profile to this file.
    #[structopt(
        short = "o",
        long = "out",
        default_value = "profile.json",
        parse(from_os_str)
    )]
    output_file: PathBuf,

    /// If neither --launch nor --serve are specified, profile this command.
    #[structopt(subcommand)]
    rest: Option<Subcommands>,

    /// Don't record. Instead, launch the profiler with the selected file in your default browser.
    #[structopt(short = "l", long = "launch", parse(from_os_str))]
    file_to_launch: Option<PathBuf>,

    /// Don't record. Instead, serve the selected file from a local webserver.
    #[structopt(short = "s", long = "serve", parse(from_os_str))]
    file_to_serve: Option<PathBuf>,
}

#[derive(Debug, PartialEq, StructOpt)]
enum Subcommands {
    #[structopt(external_subcommand)]
    Command(Vec<String>),
}

fn main() -> Result<(), MachError> {
    let opt = Opt::from_args();
    let open_in_browser = opt.file_to_launch.is_some();
    let file_for_launching_or_serving = opt.file_to_launch.or(opt.file_to_serve);
    if let Some(file) = file_for_launching_or_serving {
        start_server_main(&file, open_in_browser);
        return Ok(());
    }
    if let Some(Subcommands::Command(command)) = opt.rest {
        if !command.is_empty() {
            let time_limit = opt.time_limit.map(|secs| Duration::from_secs_f64(secs));
            let interval = Duration::from_secs_f64(opt.interval);
            start_recording(
                &opt.output_file,
                &command,
                time_limit,
                interval,
                opt.launch_when_done,
            )?;
            return Ok(());
        }
    }
    println!("Error: missing command\n");
    Opt::clap().print_help().unwrap();
    std::process::exit(1);
}

#[tokio::main]
async fn start_server_main(file: &Path, open_in_browser: bool) {
    start_server(file, open_in_browser).await;
}

fn start_recording(
    output_file: &Path,
    args: &[String],
    time_limit: Option<Duration>,
    interval: Duration,
    launch_when_done: bool,
) -> Result<(), MachError> {
    let command_name = args.first().unwrap();
    let command = which(command_name).expect("Couldn't resolve command name");
    let args: Vec<&str> = args.iter().skip(1).map(std::ops::Deref::deref).collect();

    let (task_sender, task_receiver) = unbounded();
    let sampler = Sampler::new(task_receiver, interval, time_limit);

    let mut launcher = ProcessLauncher::new(&command, &args)?;
    let child_pid = launcher.get_id();
    let child_task = launcher.take_task();
    println!("child PID: {}, childTask: {}\n", child_pid, child_task);

    let task_profiler = TaskProfiler::new(
        child_task,
        child_pid,
        Instant::now(),
        command_name,
        interval,
    )
    .expect("couldn't create TaskProfiler");
    launcher.start_execution();

    task_sender
        .send(task_profiler)
        .expect("couldn't send task to sampler");

    let profile_builder = sampler.run().expect("Sampler ran into an error");

    let file = File::create(output_file).unwrap();
    to_writer(file, &profile_builder.to_json()).expect("Couldn't write JSON");
    // println!("profile: {:?}", profile_builder);

    if launch_when_done {
        start_server_main(output_file, true);
    }

    let _exit_code = launcher.wait().expect("couldn't wait for child");

    Ok(())
}
