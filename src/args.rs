use clap::{Parser, Subcommand};

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Path to the config file
    #[clap(default_value = "config.json", long = "config")]
    pub config_path: std::path::PathBuf,
    #[clap(subcommand)]
    pub subcommand: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Import raw logs from a folder in justlog's layout (`<channel id>/<year>/<month>/<day>/channel.txt`),
    /// for example logs exported with the `?raw` parameter. The lines use Twitch's IRC format.
    Migrate {
        /// The logs folder
        #[clap(short, long, value_parser)]
        source_dir: String,
        /// List of channel ids to migrate (None specified = migrate all)
        #[clap(short, long, value_parser)]
        channel_id: Vec<String>,
        /// Parallel migration jobs
        #[clap(short, long, default_value_t = 1)]
        jobs: usize,
    },
}
