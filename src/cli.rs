use std::path::PathBuf;

#[derive(Debug, PartialEq)]
// A single startup value; boxing adds an unnecessary allocation.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Help,
    Version,
    Run(Options),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub test: String,
    pub level: String,
    pub isa: String,
    pub stores: Option<crate::portable::Stores>,
    pub threads: Option<usize>,
    pub size: Option<usize>,
    pub latency_size: Option<usize>,
    pub stride: Option<usize>,
    pub window: Option<usize>,
    pub seed: Option<u64>,
    pub samples: Option<usize>,
    pub minimum: Option<f64>,
    pub cpus: Option<Vec<usize>>,
    pub profile: Option<PathBuf>,
    pub save_profile: Option<PathBuf>,
    pub quick: bool,
    pub json: bool,
    pub gui: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            test: "all".into(),
            level: "all".into(),
            isa: "auto".into(),
            stores: None,
            threads: None,
            size: None,
            latency_size: None,
            stride: None,
            window: None,
            seed: None,
            samples: None,
            minimum: None,
            cpus: None,
            profile: None,
            save_profile: None,
            quick: false,
            json: false,
            gui: false,
        }
    }
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    let mut explicit_cli = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "--quick" => {
                options.quick = true;
                continue;
            }
            "--json" => {
                options.json = true;
                continue;
            }
            "--gui" => {
                options.gui = true;
                continue;
            }
            "--cli" => {
                explicit_cli = true;
                continue;
            }
            "-t" | "--test" | "-j" | "--threads" | "--size-mib" | "--latency-size-mib"
            | "--stride" | "--window-kib" | "--seed" | "--samples" | "--min-time" | "--cpus"
            | "--profile" | "--save-profile" | "--isa" | "--level" | "--stores" => {}
            _ => return Err(format!("unknown option '{argument}'")),
        }
        let value = args
            .next()
            .ok_or_else(|| format!("'{argument}' needs a value"))?;
        match argument.as_str() {
            "-t" | "--test" => options.test = value,
            "--level" => options.level = value,
            "--stores" => options.stores = Some(crate::portable::Stores::parse(&value)?),
            "--isa" => {
                if !matches!(value.as_str(), "auto" | "sse2" | "avx2" | "neon") {
                    return Err("--isa must be auto, sse2, avx2, or neon".into());
                }
                options.isa = value;
            }
            "--profile" => options.profile = Some(value.into()),
            "--save-profile" => options.save_profile = Some(value.into()),
            "--seed" => {
                options.seed = Some(value.parse().map_err(|_| "--seed needs a u64 integer")?)
            }
            "--min-time" => {
                let seconds: f64 = value.parse().map_err(|_| "invalid --min-time")?;
                if !seconds.is_finite() || !(0.001..=10.0).contains(&seconds) {
                    return Err("--min-time must be between 0.001 and 10 seconds".into());
                }
                options.minimum = Some(seconds);
            }
            "--cpus" => {
                let ids: Result<Vec<usize>, _> = value.split(',').map(str::parse).collect();
                let ids = ids.map_err(|_| "--cpus needs comma-separated CPU IDs")?;
                if ids.is_empty() || ids.iter().enumerate().any(|(i, id)| ids[..i].contains(id)) {
                    return Err("--cpus must contain distinct CPU IDs".into());
                }
                options.cpus = Some(ids);
            }
            _ => {
                let number: usize = value
                    .parse()
                    .map_err(|_| format!("'{argument}' needs a non-negative integer"))?;
                let bytes = |unit| {
                    number
                        .checked_mul(unit)
                        .ok_or_else(|| format!("'{argument}' is too large"))
                };
                match argument.as_str() {
                    "-j" | "--threads" => options.threads = Some(number),
                    "--size-mib" => options.size = Some(bytes(1 << 20)?),
                    "--latency-size-mib" => options.latency_size = Some(bytes(1 << 20)?),
                    "--stride" => options.stride = Some(number),
                    "--window-kib" => options.window = Some(bytes(1024)?),
                    "--samples" => options.samples = Some(number),
                    _ => unreachable!(),
                }
            }
        }
    }
    if options.gui && explicit_cli {
        return Err("--gui and --cli cannot be combined".into());
    }
    if options.gui && !cfg!(feature = "gui") {
        return Err(
            "GUI support is unavailable in this build; use --cli or build with the gui feature"
                .into(),
        );
    }
    if !matches!(
        options.test.as_str(),
        "all" | "read" | "write" | "copy" | "latency"
    ) {
        return Err("--test must be all, read, write, copy, or latency".into());
    }
    if !matches!(
        options.level.as_str(),
        "all" | "memory" | "l1" | "l2" | "l3"
    ) {
        return Err("--level must be all, memory, l1, l2, or l3".into());
    }
    if options.gui && options.json {
        return Err("--gui and --json cannot be combined".into());
    }
    if options.quick && options.profile.is_some() {
        return Err(
            "--quick and --profile cannot be combined; profiles already fix the settings".into(),
        );
    }
    Ok(Action::Run(options))
}

pub fn help() {
    let gui_help = if cfg!(feature = "gui") {
        "      --gui                   Open desktop window (default with no arguments)\n"
    } else {
        ""
    };
    println!(
        r#"memtest
Memory and cache read/write/copy bandwidth and latency.

Usage: memtest [OPTIONS]

Run
  -t, --test <TEST>           all, read, write, copy, latency [all]
      --level <LEVEL>         all, memory, l1, l2, l3 [all]
  -j, --threads <N>           Workers; 0 calibrates [0]
      --cpus <IDS>            Worker CPU IDs, comma-separated
      --quick                 Short run with small buffers and one worker
      --json                  Emit results and settings as JSON
      --samples <N>           Measured samples [7]
      --min-time <SECONDS>    Sample duration target [0.2]
      --isa <BACKEND>         auto, sse2, avx2, neon; bandwidth code path [auto]
      --stores <MODE>         streaming or cached RAM writes/copies [streaming]

Memory
      --size-mib <N>          RAM MiB per stream across workers [auto]
      --latency-size-mib <N>  Latency working set; 0 detects [0]
      --stride <BYTES>        Pointer spacing [128]
      --window-kib <N>        Randomized locality blocks; 0 shuffles all [2048]
      --seed <N>              Pointer-chain random seed [1]

Profiles
      --save-profile <FILE>   Save resolved settings
      --profile <FILE>        Replay settings; explicit options override

General
{gui_help}      --cli                   Run in the terminal
  -h, --help                  Show help
  -V, --version               Show version

Examples
  memtest
  memtest --save-profile desktop.profile
  memtest --profile desktop.profile
  memtest --threads 1 --size-mib 256 --latency-size-mib 512

Streaming uses non-temporal stores on x86, STNP hints on ARM.
Copy counts bytes read + written and uses two --size-mib RAM buffers.
Cache bandwidth uses private buffers across cores, with cached stores.
Cache footprints share half each cache's capacity; latency uses one worker.
Latency uses dependent loads. Missing cache levels are omitted.
Results are medians; range is 100 * (maximum - minimum) / median.
Bandwidth uses decimal MB/s. Buffer sizes use MiB/KiB.
Automatic worker calibration adds pilot time; fixed settings skip it.
Profiles fix RAM workloads; cache footprints and CPU placement follow the host."#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn arguments(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }
    #[test]
    fn preserves_explicit_settings_and_profiles() {
        let Action::Run(o) = parse(arguments(
            "--json --profile fixed.profile --size-mib 384 --threads 3 --cpus 0,2,4 --seed 42",
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (o.size, o.threads, o.seed),
            (Some(384 << 20), Some(3), Some(42))
        );
        assert_eq!(o.cpus, Some(vec![0, 2, 4]));
        assert_eq!(o.profile, Some(PathBuf::from("fixed.profile")));
        assert!(o.json);
    }
    #[test]
    fn rejects_invalid_cli() {
        for input in [
            "--unknown",
            "--threads",
            "--threads -1",
            "--min-time NaN",
            "--min-time 0",
            "--test stress",
            "--level l4",
            "--level",
            "--gui --json",
            "--cpus 0,0",
            "--quick --profile x",
            "--isa avx512",
            "--isa",
        ] {
            assert!(parse(arguments(input)).is_err(), "{input}");
        }
        assert!(parse(arguments(&format!("--size-mib {}", usize::MAX))).is_err());
    }
    #[test]
    fn help_and_version_are_immediate() {
        assert_eq!(parse(arguments("-h")).unwrap(), Action::Help);
        assert_eq!(parse(arguments("--version")).unwrap(), Action::Version);
    }
    #[test]
    fn cache_levels_and_copy_parse() {
        for level in ["all", "memory", "l1", "l2", "l3"] {
            let Action::Run(o) = parse(arguments(&format!("--level {level} --test copy"))).unwrap()
            else {
                panic!()
            };
            assert_eq!(o.level, level);
            assert_eq!(o.test, "copy");
        }
    }
    #[test]
    fn backend_names_parse_and_default_to_auto() {
        assert_eq!(Options::default().isa, "auto");
        for name in ["auto", "sse2", "avx2", "neon"] {
            let Action::Run(options) = parse(arguments(&format!("--isa {name}"))).unwrap() else {
                panic!()
            };
            assert_eq!(options.isa, name);
        }
        let Action::Run(options) =
            parse(arguments("--isa sse2 --isa auto --profile fixed")).unwrap()
        else {
            panic!()
        };
        assert_eq!(options.isa, "auto");
        assert_eq!(options.profile, Some("fixed".into()));
    }
    #[test]
    fn launch_modes_preserve_cli_settings_and_reject_conflicts() {
        let Action::Run(empty) = parse(Vec::<String>::new()).unwrap() else {
            panic!()
        };
        assert!(!empty.gui);
        let Action::Run(cli) = parse(arguments("--cli --quick --threads 2")).unwrap() else {
            panic!()
        };
        assert!(!cli.gui);
        assert!(cli.quick);
        assert_eq!(cli.threads, Some(2));
        for input in ["--gui --cli", "--cli --gui"] {
            assert_eq!(
                parse(arguments(input)).unwrap_err(),
                "--gui and --cli cannot be combined"
            );
        }
        #[cfg(feature = "gui")]
        {
            let Action::Run(gui) = parse(arguments("--gui --size-mib 64")).unwrap() else {
                panic!()
            };
            assert!(gui.gui);
            assert_eq!(gui.size, Some(64 << 20));
        }
        #[cfg(not(feature = "gui"))]
        assert!(
            parse(arguments("--gui"))
                .unwrap_err()
                .contains("GUI support is unavailable")
        );
    }
}
