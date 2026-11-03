use agentkeeper_core::compose;
fn main() {
    let once = compose::upsert_block("# mine\n\nlocal edit\n", "agents", "# mine\n\nlocal edit\n");
    println!("after first: {:?}", once);
    let hacked = once.replace("local edit", "hacked edit");
    println!("hacked: {:?}", hacked);
    let plan_has_foreign = agentkeeper_core::compose::is_foreign_edit(&hacked, "agents", "# mine\n\nlocal edit\n");
    println!("is_foreign_edit: {}", plan_has_foreign);
}
