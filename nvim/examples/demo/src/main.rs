fn main() {
    let mut names = vec![String::from("Ada")];
    let mut scores = vec![10, 20];

    let selected = &mut names[0];
    selected.push_str(" Lovelace");
    scores.push(30);

    println!("Names: {names:?}");
    println!("Scores: {scores:?}");
}
