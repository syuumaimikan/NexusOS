use std::{env, fs};
fn main() {
    let mut args = env::args().skip(1);
    let file = args.next().expect("IVRS file");
    let endpoint = args
        .next()
        .map(|id| u16::from_str_radix(id.trim_start_matches("0x"), 16).expect("hex requester ID"));
    let bytes = fs::read(file).expect("IVRS bytes");
    let table = nexus_nvidia::amdvi::Ivrs::parse(&bytes).expect("bounded/checksummed IVRS");
    let mut matches = 0;
    for unit in table.units() {
        println!(
            "AMD-Vi {:04x}:{:04x}: MMIO={:#x}, capability={:#x}",
            unit.segment(),
            unit.device(),
            unit.base(),
            unit.capability()
        );
        if let Some(endpoint) = endpoint {
            let requester = unit
                .requester(endpoint)
                .expect("supported requester routing");
            println!("  endpoint {endpoint:04x} -> {requester:?}");
            if requester == Some(endpoint) {
                matches += 1;
            }
        }
    }
    if endpoint.is_some() && matches == 0 {
        panic!("no direct unit route for target endpoint");
    }
    println!("PASS: native IVRS parser; firmware inspection only, no device writes");
}
