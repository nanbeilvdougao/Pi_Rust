//! Helpers shared between Pi Rust benchmarks.

use pi_core::{Message, Role};

pub fn synthetic_messages(n: usize, length: usize) -> Vec<Message> {
    let body: String = "你好 hello world "
        .repeat(length / 17 + 1)
        .chars()
        .take(length)
        .collect();
    (0..n)
        .map(|i| {
            let role = match i % 4 {
                0 => Role::System,
                1 => Role::User,
                2 => Role::Assistant,
                _ => Role::Tool,
            };
            Message::new(role, &body)
        })
        .collect()
}
