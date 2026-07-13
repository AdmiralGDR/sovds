//! Notification adapter: turn a supervisor [`Reason`] into a human-legible push
//! via ntfy, sent with `curl`.
//!
//! Notification is a courtesy, not a dependency (the principle: graceful
//! degradation). The [`Notifier`] impl still returns a `Result`, but the caller
//! is expected to log and continue — a missed push must never disturb the core.

use crate::os::{CommandRunner, RunError};
use sovereign_core::error::ActError;
use sovereign_core::failure::FailureClass;
use sovereign_core::fsm::Reason;
use sovereign_core::ports::Notifier;
use std::time::Duration;

/// A rendered, human-facing note ready to hand to ntfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    /// ntfy priority, 1 (min) .. 5 (max).
    pub priority: u8,
    pub tags: String,
    pub body: String,
}

/// Render a transition reason into plain language a non-technical person can act
/// on. Pure and total, so every phrasing is unit-testable.
#[must_use]
pub fn render(reason: Reason) -> Note {
    match reason {
        Reason::Nominal => Note {
            title: "Сеть в норме".to_string(),
            priority: 2,
            tags: "white_check_mark".to_string(),
            body: "Всё работает штатно.".to_string(),
        },
        Reason::FirstSuspicion(class) => Note {
            title: "Замечен возможный сбой".to_string(),
            priority: 2,
            tags: "eyes".to_string(),
            body: format!("Наблюдаю: {}. Пока не вмешиваюсь.", fault_ru(class)),
        },
        Reason::AwaitingConfirmation { failing, seen } => Note {
            title: "Подтверждаю сбой".to_string(),
            priority: 2,
            tags: "eyes".to_string(),
            body: format!(
                "{} — повтор {}. Жду подтверждения.",
                fault_ru(failing),
                seen
            ),
        },
        Reason::Recovered => Note {
            title: "Связь восстановлена".to_string(),
            priority: 3,
            tags: "white_check_mark".to_string(),
            body: "Сбой устранён, сеть снова в норме.".to_string(),
        },
        Reason::Confirmed { failing, level } => Note {
            title: "Устраняю сбой".to_string(),
            priority: 4,
            tags: "warning".to_string(),
            body: format!("{}: применяю меру (уровень {}).", fault_ru(failing), level),
        },
        Reason::CoolingDown => Note {
            title: "Ожидание".to_string(),
            priority: 2,
            tags: "hourglass".to_string(),
            body: "Мера применена, жду результата.".to_string(),
        },
        Reason::Escalated { failing, level } => Note {
            title: "Усиливаю меры".to_string(),
            priority: 5,
            tags: "rotating_light".to_string(),
            body: format!(
                "{} не устранён: перехожу к уровню {}.",
                fault_ru(failing),
                level
            ),
        },
    }
}

/// Human label for a fault, in the user's language.
fn fault_ru(class: FailureClass) -> &'static str {
    match class {
        FailureClass::None => "нет сбоя",
        FailureClass::WanDown => "нет связи с провайдером (WAN)",
        FailureClass::SingboxDead => "туннель остановлен",
        FailureClass::NodeDead => "узел туннеля недоступен",
        FailureClass::NodeDegraded => "узел туннеля тормозит",
        FailureClass::DnsFail => "не работает DNS",
    }
}

/// Build the `curl` argv that publishes `note` to the ntfy `topic`. Pure, so the
/// exact request is testable without hitting the network.
#[must_use]
pub fn build_ntfy_argv(topic: &str, note: &Note, timeout_s: u32) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    for part in ["curl", "-s", "-m"] {
        argv.push(part.to_string());
    }
    argv.push(timeout_s.to_string());
    argv.push("-H".to_string());
    argv.push(format!("Title: {}", note.title));
    argv.push("-H".to_string());
    argv.push(format!("Priority: {}", note.priority));
    argv.push("-H".to_string());
    argv.push(format!("Tags: {}", note.tags));
    argv.push("-d".to_string());
    argv.push(note.body.clone());
    argv.push(format!("https://ntfy.sh/{topic}"));
    argv
}

/// Publishes notes to an ntfy topic through an injected runner.
#[derive(Debug, Clone)]
pub struct NtfyNotifier<'r, R: CommandRunner> {
    runner: &'r R,
    topic: String,
    timeout: Duration,
}

impl<'r, R: CommandRunner> NtfyNotifier<'r, R> {
    #[must_use]
    pub fn new(runner: &'r R, topic: String, timeout: Duration) -> Self {
        Self {
            runner,
            topic,
            timeout,
        }
    }
}

impl<R: CommandRunner> Notifier for NtfyNotifier<'_, R> {
    fn notify(&self, event: Reason) -> Result<(), ActError> {
        let note = render(event);
        let timeout_s = u32::try_from(self.timeout.as_secs()).unwrap_or(u32::MAX);
        let argv = build_ntfy_argv(&self.topic, &note, timeout_s);
        match self.runner.run(&argv, &[], None, self.timeout) {
            Ok(output) if output.success() => Ok(()),
            Ok(_) => Err(ActError::Unverified),
            Err(RunError::Timeout) => Err(ActError::Timeout),
            Err(_) => Err(ActError::Unreachable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_ntfy_argv, render};
    use sovereign_core::failure::FailureClass;
    use sovereign_core::fsm::Reason;

    #[test]
    fn escalation_is_the_loudest_priority() {
        let note = render(Reason::Escalated {
            failing: FailureClass::WanDown,
            level: 2,
        });
        assert_eq!(note.priority, 5);
    }

    #[test]
    fn recovery_is_a_calm_positive_note() {
        let note = render(Reason::Recovered);
        assert_eq!(note.priority, 3);
        assert!(note.tags.contains("white_check_mark"));
    }

    #[test]
    fn argv_targets_the_topic_and_carries_headers() {
        let note = render(Reason::Confirmed {
            failing: FailureClass::SingboxDead,
            level: 0,
        });
        let argv = build_ntfy_argv("my-secret-topic", &note, 10);
        assert_eq!(
            argv.last().map(String::as_str),
            Some("https://ntfy.sh/my-secret-topic")
        );
        assert!(argv.iter().any(|a| a.starts_with("Title: ")));
        assert!(argv.iter().any(|a| a == "Priority: 4"));
    }
}
