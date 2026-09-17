#![no_std]
#![no_main]

nexus_guest::guest_main!(run);

fn run() -> ! {
    nexus_guest::say("guest: threads is not written yet");
    nexus_guest::exit_group(0)
}
