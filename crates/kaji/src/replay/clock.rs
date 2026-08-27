use chrono::Utc;

pub trait PromptClock: Send + Sync {
    fn prompt_timestamp(&self) -> String;
}

pub struct RealClock;

impl PromptClock for RealClock {
    #[allow(clippy::disallowed_methods)]
    fn prompt_timestamp(&self) -> String {
        Utc::now().format("%Y-%m-%d %H:00 %:z").to_string()
    }
}

pub struct FixedClock(String);

impl FixedClock {
    pub fn new(ts: String) -> Self {
        Self(ts)
    }
}

impl PromptClock for FixedClock {
    fn prompt_timestamp(&self) -> String {
        self.0.clone()
    }
}
