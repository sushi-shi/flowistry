fn combine(a: i32, b: i32) -> i32 { a + b }

fn independent(left: i32, right: i32) {
    let selected = left + 1;
    let unrelated = right + 2;
    let result = combine(selected, unrelated);
    println!("{result}");
}

fn arrays(source: [i32; 3], camera: [i32; 3]) {
    let coordinate = source[0];
    let angle = combine(coordinate, camera[2]);
    println!("{angle}");
}

fn conditional(flag: bool, other: i32) {
    let branch = if flag { 1 } else { 2 };
    let chosen = combine(branch, other);
    println!("{chosen}");
}

fn references(seed: &i32, other: i32) {
    let borrowed = *seed;
    let answer = combine(borrowed, other);
    println!("{answer}");
}

fn effects(left: i32, right: &mut i32) {
    let input = left + 1;
    let output = combine(input, { *right += 1; *right });
    println!("{output}");
}

fn unicode(source: i32, other: i32) {
    let café = source + 1;
    let total = combine(café, other);
    println!("{total}");
}

fn nested(source: i32, other: i32) {
    let closure = |value: i32| combine(value, other);
    println!("{}", closure(source));
}

fn main() {}
