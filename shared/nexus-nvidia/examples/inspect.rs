use std::{env, fs};
fn main() {
    let directory = env::args().nth(1).expect("firmware bundle directory");
    let root = std::path::Path::new(&directory);
    let boot = fs::read(root.join("GSPBOOT.BIN")).expect("GSPBOOT.BIN");
    let fmc = fs::read(root.join("FMC.BIN")).expect("FMC.BIN");
    let (boot, fmc) =
        nexus_nvidia::verify_pinned(&boot, &fmc).expect("pinned production firmware validation");
    println!("PASS: NVIDIA {} GB202/GB206 production bundle: boot={} code={} data={} manifest={}, FMC={} signature={} key={}",
        nexus_nvidia::VERSION, boot.image.len(), boot.code.size, boot.data.size, boot.manifest.size,
        fmc.image.len(),fmc.signature.len(),fmc.public_key.len());
}
