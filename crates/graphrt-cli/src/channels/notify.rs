//! Notifications through a messaging channel (`notify` in the channels file):
//! "run failed" and "needs approval" for any run of the server, in Slack,
//! Teams or Telegram instead of (or next to) Web Push.
//!
//! A target that is also one of the channel's `approvers` gets the approval as
//! a question with buttons, and their tap resumes the run. Everyone else gets
//! a short notice that points to the approvals inbox.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::store::RunStatus;

use super::{Bot, Channels, is_channel_conversation};
use crate::approvals::requests_of;
use crate::notify::{Notice, NoticeKind, Notifier, NotifyError};

/// One channel that tells its `notify` targets.
pub(crate) struct ChannelNotifier {
    channels: Arc<Channels>,
    bot: Arc<Bot>,
}

impl std::fmt::Debug for ChannelNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelNotifier").field("channel", &self.bot.name()).finish_non_exhaustive()
    }
}

impl Channels {
    /// A notifier for every channel with `notify` targets.
    pub fn notifiers(self: &Arc<Self>) -> Vec<Arc<dyn Notifier>> {
        self.bots
            .iter()
            .filter(|bot| !bot.config.notify.is_empty())
            .map(|bot| {
                Arc::new(ChannelNotifier { channels: Arc::clone(self), bot: Arc::clone(bot) }) as Arc<dyn Notifier>
            })
            .collect()
    }
}

impl ChannelNotifier {
    /// The notice as one short message.
    fn text(&self, notice: &Notice) -> String {
        let mut text = format!("**{}**\n{}\nRun `{}`", notice.title, notice.body, notice.run_id);
        if notice.kind == NoticeKind::ApprovalNeeded {
            text.push_str("\nAnswer it in the approvals inbox");
            if let Some(url) = self.channels.public_url.as_deref() {
                text.push_str(&format!(": {url}/monitor/"));
            }
            text.push('.');
        }
        text
    }

    /// Whether the channel's own question flow asks this run's approvers
    /// already: a run of one of its own conversations.
    fn asked_by_the_channel(&self, session_id: Option<&str>) -> bool {
        let own = format!("{}{}-", super::CONVERSATION_PREFIX, self.bot.name());
        session_id.is_some_and(|id| is_channel_conversation(id) && id.starts_with(&own))
    }

    async fn deliver(&self, notice: &Notice) -> Result<(), NotifyError> {
        let bot = &self.bot;
        let mut approver_targets: Vec<String> = Vec::new();
        if notice.kind == NoticeKind::ApprovalNeeded
            && let Some(state) = self.channels.state()
            && let Ok(Some(record)) = state.store().get(&notice.run_id).await
            && record.status == RunStatus::Paused
        {
            let targets: Vec<String> =
                bot.config.notify.iter().filter(|target| bot.config.approvers.contains(target)).cloned().collect();
            if self.asked_by_the_channel(record.session_id.as_deref()) {
                // They have the question with its buttons from the conversation already.
                approver_targets = targets;
            } else if !targets.is_empty() {
                let requests = requests_of(&record);
                let heading = format!("**{}**", notice.title);
                if self.channels.offer(bot, &record, &requests, &heading, &targets).await {
                    approver_targets = targets;
                }
            }
        }
        let text = self.text(notice);
        let mut reached = 0;
        for target in bot.config.notify.iter().filter(|target| !approver_targets.contains(target)) {
            let chat = match bot.channel.direct_chat(target).await {
                Ok(chat) => chat,
                Err(error) => {
                    eprintln!("warning: channel `{}`: cannot write to a notify target: {error}", bot.name());
                    continue;
                }
            };
            if bot.say(&chat, &text, &[]).await {
                reached += 1;
            }
        }
        if reached == 0 && approver_targets.is_empty() {
            return Err(NotifyError(format!("channel `{}` reached none of its notify targets", bot.name())));
        }
        Ok(())
    }
}

impl Notifier for ChannelNotifier {
    fn name(&self) -> &'static str {
        self.bot.config.kind.as_str()
    }

    fn notify<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<(), NotifyError>> {
        Box::pin(self.deliver(notice))
    }
}
