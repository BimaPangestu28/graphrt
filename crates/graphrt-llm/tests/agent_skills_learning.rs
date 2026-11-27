//! The `agent` node with skills and learning together, against a real skill
//! store, a real lesson store and a scripted model: a run that reads learned
//! notes is told they still apply when it loads a skill.

#![cfg(all(feature = "mock", feature = "skills", feature = "learning"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::NodeOutcome;
use graphrt_core::{Node, NodeContext, Services, Value, VariablePool};
use graphrt_learn::{LessonKind, LessonStore, NewLesson};
use graphrt_llm::mock::MockModel;
use graphrt_llm::nodes::agent::AgentNode;
use graphrt_llm::{ChatModel, ChatRequest, Message, ModelRegistry, Role};
use graphrt_skill::{InstallOptions, SkillSet, SkillStore};
use serde_json::json;

const AGENT: &str = "order-helper";
const STILL_APPLY: &str = "the learned notes in your instructions still apply";

struct World {
    root: PathBuf,
    skills: Arc<SkillStore>,
    lessons: Arc<LessonStore>,
    model: Arc<MockModel>,
}

impl World {
    /// A world with the skill `shipping` installed and a model that loads it, then answers.
    fn new() -> Self {
        let root = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-agent-skills-learning").unwrap();
        let dir = root.join("src").join("shipping");
        std::fs::create_dir_all(&dir).unwrap();
        let text = "---\nname: shipping\ndescription: Use when a customer asks about the shipping cost.\n---\n\
                    End the reply with Shipping code ONGKIR-7.\n";
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
        let skills = Arc::new(SkillStore::open(root.join("skills")));
        skills.install(&dir, InstallOptions::default()).unwrap();
        let model = Arc::new(MockModel::new(vec![
            MockModel::tool_calls(&[("call-1", "load_skill", r#"{"name":"shipping"}"#)]),
            MockModel::text("Free inside Java. Shipping code ONGKIR-7."),
        ]));
        Self { lessons: Arc::new(LessonStore::open(root.join("learning"))), skills, model, root }
    }

    fn teach(&self, text: &str) {
        let new = NewLesson { agent: AGENT.into(), user: None, kind: LessonKind::Preference, text: text.into() };
        self.lessons.add(new, "owner").unwrap();
    }

    /// Run the agent and return the two requests the model saw: the one it answered
    /// by loading the skill, and the one after the skill was loaded.
    async fn run(&self, node: &AgentNode) -> Vec<ChatRequest> {
        let services = Services::new()
            .with(ModelRegistry::new().with("openai", Arc::clone(&self.model) as Arc<dyn ChatModel>))
            .with(SkillSet::new(Arc::clone(&self.skills)).into_service())
            .with(Arc::clone(&self.lessons).into_service());
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("How much is shipping to Surabaya?"));
        let ctx = NodeContext::for_test("agent", pool).with_services(Arc::new(services));
        match node.run(ctx).await.unwrap() {
            NodeOutcome::Completed(_) => {}
            NodeOutcome::Paused(pause) => panic!("unexpected pause: {pause:?}"),
        }
        self.model.requests()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn agent(learning: bool) -> AgentNode {
    let mut config = json!({"model": {"provider": "openai", "name": "gpt"}, "skills": ["shipping"]});
    if learning {
        config.as_object_mut().unwrap().insert("learning".into(), json!({"agent": AGENT}));
    }
    AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap()
}

/// What the model was shown as the result of loading the skill.
fn loaded_skill(request: &ChatRequest) -> String {
    request.messages.iter().rfind(|message| message.role == Role::Tool).map(Message::text).unwrap_or_default()
}

#[tokio::test]
async fn a_run_that_reads_notes_is_told_they_still_apply_when_it_loads_a_skill() {
    let world = World::new();
    world.teach("Also tell the customer that delivery takes 2 to 4 days.");
    let requests = world.run(&agent(true)).await;
    assert_eq!(requests.len(), 2);
    let system = requests[1].messages.first().map(Message::text).unwrap_or_default();
    assert!(system.contains("delivery takes 2 to 4 days"), "{system}");
    let skill = loaded_skill(&requests[1]);
    assert!(skill.contains("End the reply with Shipping code ONGKIR-7."), "{skill}");
    // The reminder follows the skill and stands outside its content block.
    let after_skill = skill.rsplit("</skill_content>").next().unwrap_or_default();
    assert!(after_skill.contains(STILL_APPLY), "{skill}");
    assert!(after_skill.contains("Keep every step of the skill"), "{skill}");
}

#[tokio::test]
async fn a_loaded_skill_says_nothing_about_notes_when_the_run_reads_none() {
    // Learning on, nothing approved yet.
    let world = World::new();
    let requests = world.run(&agent(true)).await;
    assert!(!loaded_skill(&requests[1]).contains(STILL_APPLY));

    // Notes exist, but this agent does not read them.
    let world = World::new();
    world.teach("Also tell the customer that delivery takes 2 to 4 days.");
    let requests = world.run(&agent(false)).await;
    assert!(!loaded_skill(&requests[1]).contains(STILL_APPLY));
}
