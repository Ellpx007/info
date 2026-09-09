use info::net;

fn main() {
    let snapshot = net::collect();
    net::print(&snapshot);
}
