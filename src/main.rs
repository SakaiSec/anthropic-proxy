fn main() {
    anthropic_proxy::run(&std::env::args().collect::<Vec<_>>());
}