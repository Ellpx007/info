use info::disk;
use info::ui;

fn main() {
    let disks = disk::collect_disks();
    ui::print_disk_usage_table(&disks);
}
