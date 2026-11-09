//! Write a deterministic fixture image as a docker-save tarball (same bytes every run).
//!   cargo run --release --example fixture -- <seed> <layers> <bytes-per-layer> <out.tar> [os/arch]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: fixture <seed> <layers> <bytes-per-layer> <out.tar> [os/arch]");
        std::process::exit(2);
    }
    let (seed, layers, size) = (
        a[1].parse().unwrap(),
        a[2].parse().unwrap(),
        a[3].parse().unwrap(),
    );
    let plat = a.get(5).map(String::as_str).unwrap_or("linux/amd64");
    let (os, arch) = plat.split_once('/').expect("os/arch");
    let img = crane::fixture::Image::new(seed, layers, size, os, arch);
    std::fs::write(
        &a[4],
        crane::fixture::docker_tarball(&[("fixture/image:latest", &img)]),
    )
    .unwrap();
}
