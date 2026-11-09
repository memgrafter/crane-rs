//! Argument parsing and dispatch. Same command names, argument order and flag names
//! as go crane for the v1 subset. stdout carries only results; errors go to stderr.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::commands::{self, Ctx};
use crate::registry;

#[derive(Debug, Parser)]
#[command(
    name = "crane",
    about = "crane is a tool for managing container images",
    disable_version_flag = true
)]
pub struct Cli {
    /// Allow image references to be fetched without TLS
    #[arg(long, global = true)]
    pub insecure: bool,
    /// Specifies the platform in the form os/arch[/variant][:osversion] (e.g. linux/amd64). (default all)
    #[arg(long, global = true)]
    pub platform: Option<String>,
    /// Enable debug logs
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Tarball,
    Oci,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Push local image contents to a remote registry
    Push {
        /// docker-save tarball or OCI image layout directory
        path: PathBuf,
        reference: String,
        /// Push the OCI layout's index.json as an index, even with one image
        #[arg(long)]
        index: bool,
    },
    /// Pull remote images by reference and store their contents locally
    Pull {
        #[arg(required = true, num_args = 2..)]
        args: Vec<String>,
        /// Format in which to save images ("tarball" or "oci")
        #[arg(long, value_enum, default_value = "tarball")]
        format: Format,
        /// Annotate each image in an OCI layout with its reference
        #[arg(long = "annotate-ref")]
        annotate_ref: bool,
    },
    /// Efficiently tag a remote image
    Tag { image: String, tag: String },
    /// Efficiently copy a remote image from src to dst while retaining the digest value
    #[command(alias = "cp")]
    Copy { src: String, dst: String },
    /// Get the digest of an image
    Digest {
        image: String,
        /// Print the full image reference by digest
        #[arg(long = "full-ref")]
        full_ref: bool,
    },
    /// Get the manifest of an image
    Manifest { image: String },
    /// Get the config of an image
    Config { image: String },
    /// Read a blob from the registry
    Blob { blob: String },
    /// List the tags in a repo
    Ls {
        repo: String,
        /// (Optional) if true, print the full image reference
        #[arg(long = "full-ref")]
        full_ref: bool,
        /// (Optional) if true, omit digest tags (e.g., ':sha256-...')
        #[arg(short = 'O', long = "omit-digest-tags")]
        omit_digest_tags: bool,
    },
    /// List the repos in a registry
    Catalog {
        registry: String,
        /// (Optional) if true, print the full image reference
        #[arg(long = "full-ref")]
        full_ref: bool,
    },
    /// Delete an image reference from its registry
    Delete { image: String },
    /// Registry server
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Print the version
    Version,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// Serve a registry on a directory (default 127.0.0.1:$PORT or 5050)
    Serve {
        /// Address to listen on (host:port)
        #[arg(long)]
        address: Option<String>,
        /// Path to a directory where blobs will be stored
        #[arg(long)]
        disk: Option<PathBuf>,
    },
}

/// Parse `args` (including argv[0]) and run. Returns the process exit code.
pub fn run<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { 1 } else { 0 };
            let text = e.render().to_string();
            let _ = if code == 0 {
                out.write_all(text.as_bytes())
            } else {
                err.write_all(text.as_bytes())
            };
            return code;
        }
    };
    match dispatch(cli, out, err) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "Error: {e:#}");
            1
        }
    }
}

fn dispatch(cli: Cli, out: &mut dyn Write, err: &mut dyn Write) -> anyhow::Result<()> {
    let ctx = Ctx::new(cli.insecure, cli.platform.as_deref(), cli.verbose)?;
    match cli.command {
        Command::Push {
            path,
            reference,
            index,
        } => commands::push(&ctx, &path, &reference, index, out),
        Command::Pull {
            mut args,
            format,
            annotate_ref,
        } => {
            let path = PathBuf::from(args.pop().expect("clap requires 2 args"));
            commands::pull(&ctx, &args, &path, format, annotate_ref)
        }
        Command::Tag { image, tag } => commands::tag(&ctx, &image, &tag),
        Command::Copy { src, dst } => commands::copy(&ctx, &src, &dst),
        Command::Digest { image, full_ref } => commands::digest(&ctx, &image, full_ref, out),
        Command::Manifest { image } => commands::manifest(&ctx, &image, out),
        Command::Config { image } => commands::config(&ctx, &image, out),
        Command::Blob { blob } => commands::blob(&ctx, &blob, out),
        Command::Ls {
            repo,
            full_ref,
            omit_digest_tags,
        } => commands::ls(&ctx, &repo, full_ref, omit_digest_tags, out),
        Command::Catalog { registry, full_ref } => {
            commands::catalog(&ctx, &registry, full_ref, out)
        }
        Command::Delete { image } => commands::delete(&ctx, &image),
        Command::Registry {
            command: RegistryCommand::Serve { address, disk },
        } => registry::serve(
            &registry::ServeConfig {
                address,
                disk,
                verbose: cli.verbose,
            },
            err,
        ),
        Command::Version => {
            writeln!(out, "{}", env!("CARGO_PKG_VERSION"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut argv = vec!["crane"];
        argv.extend_from_slice(args);
        let code = run(argv, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn help_version_and_parse_errors() {
        let (code, out, _) = run_args(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("crane is a tool for managing container images"));
        assert_eq!(
            run_args(&["version"]),
            (0, format!("{}\n", env!("CARGO_PKG_VERSION")), String::new())
        );
        let (code, out, err) = run_args(&["nope"]);
        assert_eq!((code, out.is_empty()), (1, true));
        assert!(err.contains("unrecognized subcommand"));
        assert_eq!(run_args(&["pull", "only-one"]).0, 1);
        let (code, _, err) = run_args(&["digest", "--platform", "bad", "x"]);
        assert_eq!(code, 1);
        assert!(err.starts_with("Error: platform must be"), "{err}");
    }

    #[test]
    fn every_command_dispatches() {
        let r = crate::commands::tests::reg();
        let dir = tempfile::tempdir().unwrap();
        let img = crate::fixture::Image::small(3);
        let tar = dir.path().join("i.tar");
        std::fs::write(&tar, crate::fixture::docker_tarball(&[("a:b", &img)])).unwrap();
        let t = tar.to_str().unwrap();
        let a = |s: &str| s.replace("REG", &r.addr);
        let ok = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| a(s)).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (code, out, err) = run_args(&args);
            assert_eq!(code, 0, "{args:?}: {err}");
            out
        };
        let pushed = ok(&["push", t, "REG/r/i:1"]);
        let digest = pushed.trim().rsplit('@').next().unwrap().to_string();
        assert_eq!(ok(&["digest", "REG/r/i:1"]).trim(), digest);
        assert_eq!(ok(&["digest", "--full-ref", "REG/r/i:1"]), pushed);
        ok(&["manifest", "REG/r/i:1"]);
        ok(&["config", "--insecure", "-v", "REG/r/i:1"]);
        ok(&[
            "blob",
            &format!("REG/r/i@{}", crate::digest::of(&img.config)),
        ]);
        ok(&["tag", "REG/r/i:1", "2"]);
        assert_eq!(
            ok(&["ls", "-O", "--full-ref", "REG/r/i"]),
            a("REG/r/i:1\nREG/r/i:2\n")
        );
        ok(&["cp", "REG/r/i:1", "REG/r/c:1"]);
        assert_eq!(
            ok(&["catalog", "--full-ref", "REG"]),
            a("REG/r/c\nREG/r/i\n")
        );
        let out = dir.path().join("o.tar");
        ok(&[
            "pull",
            "--platform",
            "all",
            "REG/r/i:1",
            out.to_str().unwrap(),
        ]);
        ok(&[
            "pull",
            "--format",
            "oci",
            "--annotate-ref",
            "REG/r/i:1",
            dir.path().join("lay").to_str().unwrap(),
        ]);
        ok(&["delete", "REG/r/c:1"]);
        let (code, _, err) = run_args(&["manifest", &a("REG/r/c:1")]);
        assert_eq!(code, 1);
        assert!(err.starts_with("Error: GET "), "{err}");
    }

    #[test]
    fn registry_serve_dispatches() {
        // A bad address returns an error instead of serving.
        let (code, _, err) = run_args(&["registry", "serve", "--address", "not-an-address"]);
        assert_eq!(code, 1, "{err}");
        // A good one serves forever; run it on a thread and wait for the listening line.
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("d").to_str().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        struct Chan(std::sync::mpsc::Sender<String>);
        impl std::io::Write for Chan {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.send(String::from_utf8_lossy(b).into_owned());
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        std::thread::spawn(move || {
            let mut err = Chan(tx);
            run(
                [
                    "crane",
                    "registry",
                    "serve",
                    "--address",
                    "127.0.0.1:0",
                    "--disk",
                    &disk,
                ],
                &mut Vec::new(),
                &mut err,
            )
        });
        let mut line = String::new();
        while !line.ends_with('\n') {
            line.push_str(&rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
        }
        assert!(line.starts_with("listening on http://127.0.0.1:"), "{line}");
        std::mem::forget(dir); // the server thread keeps using it until the test process exits
    }
}
