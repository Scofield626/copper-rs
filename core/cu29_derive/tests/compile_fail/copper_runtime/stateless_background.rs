use cu29_derive::copper_runtime;

#[copper_runtime(config = "config/stateless_background_invalid.ron", sim_mode = true, ignore_resources = true)]
struct App {}

fn main() {}
