use super::app::{ChatLine, Sender};
use futures::StreamExt;
use std::future::Future;
use tokio::task::JoinHandle;

const CONTEXT_BYTES: usize = 8 * 1024;
const MESSAGE_BYTES: usize = 2 * 1024;
const OUTPUT_CHARS: usize = 240;

pub fn context(chat: &[ChatLine]) -> String {
    let mut recent = Vec::with_capacity(4);
    let mut used = 0;
    for line in chat.iter().rev() {
        let role = match line.sender {
            Sender::User => "User: ",
            Sender::Agent if line.tool.is_none() => "Assistant: ",
            _ => continue,
        };
        let budget = MESSAGE_BYTES.min(CONTEXT_BYTES - used - role.len() - 2);
        let mut end = line.text.len().min(budget);
        while !line.text.is_char_boundary(end) {
            end -= 1;
        }
        let text = format!("{role}{}", line.text.get(..end).expect("UTF-8 boundary"));
        used += text.len() + 2;
        recent.push(text);
        if recent.len() == 4 || used + MESSAGE_BYTES > CONTEXT_BYTES {
            break;
        }
    }
    recent.reverse();
    recent.join("\n\n")
}

pub fn display_text(text: &str) -> Option<String> {
    let first = text.trim().lines().find(|line| !line.trim().is_empty())?;
    let bounded: String = first.chars().take(OUTPUT_CHARS).collect();
    let safe = super::ui::sanitize_for_display(&bounded);
    Some(safe.chars().take(OUTPUT_CHARS).collect())
}

pub async fn collect(stream: kaji_providers::base::MessageStream) -> Option<String> {
    let mut stream = stream.take(256);
    let mut text = String::new();
    let mut chars = 0;
    while let Some(chunk) = stream.next().await {
        let (message, _) = chunk.ok()?;
        if let Some(message) = message {
            for part in message
                .content
                .iter()
                .filter_map(|content| content.as_text())
            {
                for ch in part.chars().take(OUTPUT_CHARS - chars) {
                    text.push(ch);
                    chars += 1;
                }
                if chars == OUTPUT_CHARS || text.trim_start().contains('\n') {
                    return display_text(&text);
                }
            }
        }
    }
    display_text(&text)
}

#[derive(Default)]
pub struct Suggestions {
    task: Option<JoinHandle<Option<String>>>,
}

impl Suggestions {
    pub fn start(&mut self, job: impl Future<Output = Option<String>> + Send + 'static) {
        self.cancel();
        self.task = Some(tokio::spawn(job));
    }

    pub fn pending(&self) -> bool {
        self.task.is_some()
    }

    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    pub async fn finish(&mut self) -> Option<String> {
        let result = self.task.as_mut()?.await.ok().flatten();
        self.task = None;
        result
    }
}

impl Drop for Suggestions {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(sender: Sender, text: &str) -> ChatLine {
        ChatLine {
            sender,
            text: text.to_owned(),
            tool: None,
            rendered: None,
        }
    }

    #[test]
    fn current_exchange_is_chronological_and_excludes_notices_and_reasoning() {
        let mut chat = vec![line(Sender::User, "old question")];
        chat.push(line(Sender::User, "new question"));
        chat.push(line(Sender::Thinking, "private reasoning"));
        chat.push(line(Sender::System, "tool notice"));
        chat.push(line(Sender::Agent, "new answer"));
        assert_eq!(
            context(&chat),
            "User: old question\n\nUser: new question\n\nAssistant: new answer"
        );
        chat.push(line(Sender::User, "latest question"));
        chat.push(line(Sender::Agent, "latest answer"));
        let current = context(&chat);
        assert!(!current.contains("old question"));
        assert!(current.ends_with("Assistant: latest answer"));
    }

    #[test]
    fn unicode_context_and_hostile_output_are_bounded() {
        let chat = vec![line(Sender::User, &"界".repeat(100_000)); 6];
        assert!(context(&chat).len() <= CONTEXT_BYTES);
        let text = display_text(&format!("\u{1b}[31m{}\nsecond line", "界".repeat(1000))).unwrap();
        assert!(text.chars().count() <= OUTPUT_CHARS);
        assert!(!text.contains('\u{1b}') && !text.contains('\n'));
        assert!(display_text(" \n ").is_none());
    }

    #[tokio::test]
    async fn cancellation_and_drop_release_the_owned_job() {
        for drop_owner in [false, true] {
            let mut jobs = Suggestions::default();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            jobs.start(async move {
                let _held = held_tx;
                started_tx.send(()).unwrap();
                std::future::pending().await
            });
            started_rx.await.unwrap();
            if drop_owner {
                drop(jobs);
            } else {
                jobs.cancel();
                assert!(!jobs.pending());
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), held_rx)
                    .await
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn streamed_suggestions_stop_at_the_display_budget() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = polls.clone();
        let stream = futures::stream::iter(0..1000).map(move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                Some(
                    kaji::conversation::message::Message::assistant()
                        .with_text("界".repeat(10_000)),
                ),
                None,
            ))
        });
        let text = collect(Box::pin(stream)).await.unwrap();
        assert_eq!(text.chars().count(), OUTPUT_CHARS);
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_jobs_finish_and_a_replacement_cannot_deliver_an_old_result() {
        let mut jobs = Suggestions::default();
        jobs.start(async { None });
        assert!(jobs.finish().await.is_none());
        assert!(!jobs.pending());
        jobs.start(std::future::pending());
        jobs.start(async { Some("current".to_owned()) });
        assert_eq!(jobs.finish().await.as_deref(), Some("current"));
        assert!(!jobs.pending());
    }
}
