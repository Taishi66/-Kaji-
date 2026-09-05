//! Spec YAML d'un workflow : stages ordonnés, fan-out d'agents (liste ou map
//! nommée), dépendances, gates et budgets.
//!
//! Le YAML utilisateur est lu par une couche brute privée puis converti en
//! structures typées : chaque cause de rejet a sa propre variante d'erreur,
//! plutôt qu'un message serde opaque. `Serialize` est dérivé sur les types
//! publics pour les payloads d'événements et d'IPC — l'entrée utilisateur, elle,
//! passe toujours par [`WorkflowSpec::from_yaml`].
//!
//! `Deserialize` est dérivé en regard de `Serialize`, pour relire une spec
//! **déjà validée** depuis un payload que kaji a lui-même écrit (l'événement
//! `workflow_started` du journal v2). Il court-circuite la couche brute et donc
//! la validation : ne jamais le brancher sur de l'entrée utilisateur.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub name: String,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    pub agents: Vec<AgentSpec>,
    pub depends_on: Vec<String>,
    pub gate: Gate,
    pub budgets: Budgets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    #[default]
    Auto,
    Approve,
}

impl Gate {
    pub fn label(&self) -> &'static str {
        match self {
            Gate::Auto => "auto",
            Gate::Approve => "approbation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub source: AgentSource,
    pub model: Option<String>,
    pub inputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    Recipe(PathBuf),
    Prompt(String),
}

/// Une occurrence `{{stage.agent.output}}` dans un gabarit d'entrée. Le span est
/// conservé pour que la substitution à l'exécution réécrive exactement la même
/// portion de texte que celle validée ici.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReference {
    pub span: Range<usize>,
    pub raw: String,
    pub target: Option<InputTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTarget {
    pub stage: String,
    pub agent: String,
}

#[derive(Debug)]
pub enum WorkflowSpecError {
    Yaml(serde_yaml::Error),
    EmptyWorkflowName,
    NoStages,
    EmptyStageName {
        index: usize,
    },
    DuplicateStageName {
        name: String,
    },
    StageNameContainsDot {
        name: String,
    },
    StageWithoutAgents {
        stage: String,
    },
    AgentsShape {
        stage: String,
    },
    AgentNameMissing {
        stage: String,
        index: usize,
    },
    AgentNameConflict {
        stage: String,
        key: String,
        name: String,
    },
    DuplicateAgentName {
        stage: String,
        name: String,
    },
    AgentNameContainsDot {
        stage: String,
        agent: String,
    },
    AgentSourceMissing {
        stage: String,
        agent: String,
    },
    AgentSourceAmbiguous {
        stage: String,
        agent: String,
    },
    EmptyAgentSource {
        stage: String,
        agent: String,
    },
    UnknownDependency {
        stage: String,
        depends_on: String,
    },
    SelfDependency {
        stage: String,
    },
    DependencyCycle {
        stages: Vec<String>,
    },
    InputReferenceMalformed {
        stage: String,
        agent: String,
        input: String,
        reference: String,
    },
    UnknownInputStage {
        stage: String,
        agent: String,
        input: String,
        referenced: String,
    },
    UnknownInputAgent {
        stage: String,
        agent: String,
        input: String,
        referenced_stage: String,
        referenced_agent: String,
    },
    InputReferenceNotEarlier {
        stage: String,
        agent: String,
        input: String,
        referenced_stage: String,
    },
    NonPositiveBudget {
        stage: String,
        field: &'static str,
        value: i64,
    },
}

impl fmt::Display for WorkflowSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowSpecError::Yaml(error) => write!(f, "YAML illisible : {error}"),
            WorkflowSpecError::EmptyWorkflowName => write!(f, "le workflow n'a pas de nom"),
            WorkflowSpecError::NoStages => write!(f, "le workflow ne déclare aucun stage"),
            WorkflowSpecError::EmptyStageName { index } => {
                write!(f, "le stage n°{index} n'a pas de nom")
            }
            WorkflowSpecError::DuplicateStageName { name } => {
                write!(f, "deux stages portent le nom « {name} »")
            }
            WorkflowSpecError::StageNameContainsDot { name } => write!(
                f,
                "le nom de stage « {name} » contient un point, interdit dans les identifiants"
            ),
            WorkflowSpecError::StageWithoutAgents { stage } => {
                write!(f, "le stage « {stage} » ne déclare aucun agent")
            }
            WorkflowSpecError::AgentsShape { stage } => write!(
                f,
                "les agents du stage « {stage} » ne sont ni une liste ni une map nommée"
            ),
            WorkflowSpecError::AgentNameMissing { stage, index } => write!(
                f,
                "l'agent n°{index} du stage « {stage} » n'a pas de nom"
            ),
            WorkflowSpecError::AgentNameConflict { stage, key, name } => write!(
                f,
                "l'agent « {key} » du stage « {stage} » se renomme « {name} »"
            ),
            WorkflowSpecError::DuplicateAgentName { stage, name } => write!(
                f,
                "deux agents du stage « {stage} » portent le nom « {name} »"
            ),
            WorkflowSpecError::AgentNameContainsDot { stage, agent } => write!(
                f,
                "le nom de l'agent « {agent} » du stage « {stage} » contient un point, interdit dans les identifiants"
            ),
            WorkflowSpecError::AgentSourceMissing { stage, agent } => write!(
                f,
                "l'agent « {agent} » du stage « {stage} » n'a ni recipe ni prompt"
            ),
            WorkflowSpecError::AgentSourceAmbiguous { stage, agent } => write!(
                f,
                "l'agent « {agent} » du stage « {stage} » déclare à la fois recipe et prompt"
            ),
            WorkflowSpecError::EmptyAgentSource { stage, agent } => write!(
                f,
                "la source de l'agent « {agent} » du stage « {stage} » est vide"
            ),
            WorkflowSpecError::UnknownDependency { stage, depends_on } => write!(
                f,
                "le stage « {stage} » dépend de « {depends_on} », qui n'existe pas"
            ),
            WorkflowSpecError::SelfDependency { stage } => {
                write!(f, "le stage « {stage} » dépend de lui-même")
            }
            WorkflowSpecError::DependencyCycle { stages } => {
                write!(f, "cycle de dépendances : {}", stages.join(" → "))
            }
            WorkflowSpecError::InputReferenceMalformed {
                stage,
                agent,
                input,
                reference,
            } => write!(
                f,
                "l'entrée « {input} » de « {stage}.{agent} » référence « {reference} » au lieu de stage.agent.output"
            ),
            WorkflowSpecError::UnknownInputStage {
                stage,
                agent,
                input,
                referenced,
            } => write!(
                f,
                "l'entrée « {input} » de « {stage}.{agent} » référence le stage « {referenced} », qui n'existe pas"
            ),
            WorkflowSpecError::UnknownInputAgent {
                stage,
                agent,
                input,
                referenced_stage,
                referenced_agent,
            } => write!(
                f,
                "l'entrée « {input} » de « {stage}.{agent} » référence « {referenced_agent} », absent du stage « {referenced_stage} »"
            ),
            WorkflowSpecError::InputReferenceNotEarlier {
                stage,
                agent,
                input,
                referenced_stage,
            } => write!(
                f,
                "l'entrée « {input} » de « {stage}.{agent} » référence « {referenced_stage} », qui n'est pas un ancêtre (via depends_on)"
            ),
            WorkflowSpecError::NonPositiveBudget {
                stage,
                field,
                value,
            } => write!(
                f,
                "le budget {field} du stage « {stage} » vaut {value} au lieu d'un entier positif"
            ),
        }
    }
}

impl Error for WorkflowSpecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            WorkflowSpecError::Yaml(error) => Some(error),
            _ => None,
        }
    }
}

impl WorkflowSpec {
    pub fn from_yaml(yaml: &str) -> Result<Self, WorkflowSpecError> {
        let raw: RawWorkflow = serde_yaml::from_str(yaml).map_err(WorkflowSpecError::Yaml)?;
        let spec = raw.into_spec()?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), WorkflowSpecError> {
        if self.name.trim().is_empty() {
            return Err(WorkflowSpecError::EmptyWorkflowName);
        }
        if self.stages.is_empty() {
            return Err(WorkflowSpecError::NoStages);
        }

        let mut seen_stages: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.name.trim().is_empty() {
                return Err(WorkflowSpecError::EmptyStageName { index });
            }
            if stage.name.contains('.') {
                return Err(WorkflowSpecError::StageNameContainsDot {
                    name: stage.name.clone(),
                });
            }
            if seen_stages.insert(&stage.name, index).is_some() {
                return Err(WorkflowSpecError::DuplicateStageName {
                    name: stage.name.clone(),
                });
            }
        }

        for stage in &self.stages {
            stage.validate_agents()?;
            stage.validate_budgets()?;
            self.validate_dependencies(stage, &seen_stages)?;
        }

        self.validate_acyclic(&seen_stages)?;
        self.validate_input_references(&seen_stages)
    }

    pub fn stage(&self, name: &str) -> Option<&Stage> {
        self.stages.iter().find(|stage| stage.name == name)
    }

    fn validate_dependencies(
        &self,
        stage: &Stage,
        indexes: &BTreeMap<&str, usize>,
    ) -> Result<(), WorkflowSpecError> {
        for dependency in &stage.depends_on {
            if dependency == &stage.name {
                return Err(WorkflowSpecError::SelfDependency {
                    stage: stage.name.clone(),
                });
            }
            if !indexes.contains_key(dependency.as_str()) {
                return Err(WorkflowSpecError::UnknownDependency {
                    stage: stage.name.clone(),
                    depends_on: dependency.clone(),
                });
            }
        }
        Ok(())
    }

    /// Une entrée `{{stage.agent.output}}` n'est valide que si `stage` est un
    /// ancêtre transitif via `depends_on` — la topologie du DAG fait foi, pas la
    /// position dans la liste : un `depends_on` déclaré après son stage dans le
    /// document reste légal (voir `validate_dependencies`), donc un stage doit
    /// pouvoir consommer la sortie de la dépendance même qu'il déclare. Appelée
    /// après `validate_acyclic`, donc l'ascendance se calcule sans risque de
    /// boucle infinie sur un cycle.
    fn validate_input_references(
        &self,
        indexes: &BTreeMap<&str, usize>,
    ) -> Result<(), WorkflowSpecError> {
        let ancestors: Vec<HashSet<usize>> = (0..self.stages.len())
            .map(|index| self.transitive_dependencies(index, indexes))
            .collect();

        for (index, stage) in self.stages.iter().enumerate() {
            for agent in &stage.agents {
                for (input, template) in &agent.inputs {
                    for reference in input_references(template) {
                        let Some(target) = reference.target else {
                            return Err(WorkflowSpecError::InputReferenceMalformed {
                                stage: stage.name.clone(),
                                agent: agent.name.clone(),
                                input: input.clone(),
                                reference: reference.raw,
                            });
                        };
                        let Some(&target_index) = indexes.get(target.stage.as_str()) else {
                            return Err(WorkflowSpecError::UnknownInputStage {
                                stage: stage.name.clone(),
                                agent: agent.name.clone(),
                                input: input.clone(),
                                referenced: target.stage,
                            });
                        };
                        if !ancestors[index].contains(&target_index) {
                            return Err(WorkflowSpecError::InputReferenceNotEarlier {
                                stage: stage.name.clone(),
                                agent: agent.name.clone(),
                                input: input.clone(),
                                referenced_stage: target.stage,
                            });
                        }
                        if !self.stages[target_index]
                            .agents
                            .iter()
                            .any(|candidate| candidate.name == target.agent)
                        {
                            return Err(WorkflowSpecError::UnknownInputAgent {
                                stage: stage.name.clone(),
                                agent: agent.name.clone(),
                                input: input.clone(),
                                referenced_stage: target.stage,
                                referenced_agent: target.agent,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn transitive_dependencies(
        &self,
        start: usize,
        indexes: &BTreeMap<&str, usize>,
    ) -> HashSet<usize> {
        let mut reachable = HashSet::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for dependency in &self.stages[node].depends_on {
                if let Some(&dependency_index) = indexes.get(dependency.as_str()) {
                    if reachable.insert(dependency_index) {
                        stack.push(dependency_index);
                    }
                }
            }
        }
        reachable
    }

    /// DFS itérative tricolore : un arc vers un stage encore sur la pile ferme un
    /// cycle, que l'on rend dans l'ordre de parcours pour que le message nomme la
    /// boucle plutôt qu'un seul stage.
    fn validate_acyclic(&self, indexes: &BTreeMap<&str, usize>) -> Result<(), WorkflowSpecError> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Unvisited,
            OnStack,
            Done,
        }

        let mut marks = vec![Mark::Unvisited; self.stages.len()];
        let mut path: Vec<usize> = Vec::new();

        for root in 0..self.stages.len() {
            if marks[root] != Mark::Unvisited {
                continue;
            }
            let mut stack = vec![(root, 0usize)];
            marks[root] = Mark::OnStack;
            path.push(root);

            while let Some((node, cursor)) = stack.pop() {
                let dependencies = &self.stages[node].depends_on;
                if cursor == dependencies.len() {
                    marks[node] = Mark::Done;
                    path.pop();
                    continue;
                }
                stack.push((node, cursor + 1));
                let next = indexes[dependencies[cursor].as_str()];
                match marks[next] {
                    Mark::Done => {}
                    Mark::OnStack => {
                        let start = path.iter().position(|&n| n == next).unwrap_or(0);
                        return Err(WorkflowSpecError::DependencyCycle {
                            stages: path[start..]
                                .iter()
                                .map(|&n| self.stages[n].name.clone())
                                .collect(),
                        });
                    }
                    Mark::Unvisited => {
                        marks[next] = Mark::OnStack;
                        path.push(next);
                        stack.push((next, 0));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Stage {
    pub fn agent(&self, name: &str) -> Option<&AgentSpec> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    fn validate_agents(&self) -> Result<(), WorkflowSpecError> {
        if self.agents.is_empty() {
            return Err(WorkflowSpecError::StageWithoutAgents {
                stage: self.name.clone(),
            });
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.agents.len());
        for (index, agent) in self.agents.iter().enumerate() {
            if agent.name.trim().is_empty() {
                return Err(WorkflowSpecError::AgentNameMissing {
                    stage: self.name.clone(),
                    index,
                });
            }
            if agent.name.contains('.') {
                return Err(WorkflowSpecError::AgentNameContainsDot {
                    stage: self.name.clone(),
                    agent: agent.name.clone(),
                });
            }
            if seen.contains(&agent.name.as_str()) {
                return Err(WorkflowSpecError::DuplicateAgentName {
                    stage: self.name.clone(),
                    name: agent.name.clone(),
                });
            }
            if agent.source.is_empty() {
                return Err(WorkflowSpecError::EmptyAgentSource {
                    stage: self.name.clone(),
                    agent: agent.name.clone(),
                });
            }
            seen.push(&agent.name);
        }
        Ok(())
    }

    fn validate_budgets(&self) -> Result<(), WorkflowSpecError> {
        for (field, value) in [
            ("max_tokens", self.budgets.max_tokens),
            ("max_duration_s", self.budgets.max_duration_s),
        ] {
            if let Some(value) = value {
                if value <= 0 {
                    return Err(WorkflowSpecError::NonPositiveBudget {
                        stage: self.name.clone(),
                        field,
                        value,
                    });
                }
            }
        }
        Ok(())
    }
}

impl AgentSource {
    fn is_empty(&self) -> bool {
        match self {
            AgentSource::Recipe(path) => path.to_string_lossy().trim().is_empty(),
            AgentSource::Prompt(text) => text.trim().is_empty(),
        }
    }
}

/// Les occurrences `{{…}}` d'un gabarit, dans l'ordre du texte. Une occurrence
/// dont le corps n'est pas `stage.agent.output` est rendue avec `target: None`
/// plutôt que ignorée : l'appelant décide si c'est une erreur.
pub fn input_references(template: &str) -> Vec<InputReference> {
    let bytes = template.as_bytes();
    let mut references = Vec::new();
    let mut cursor = 0;

    while cursor + 1 < bytes.len() {
        if bytes[cursor] != b'{' || bytes[cursor + 1] != b'{' {
            cursor += 1;
            continue;
        }
        let body_start = cursor + 2;
        let Some(offset) = template[body_start..].find("}}") else {
            break;
        };
        let body_end = body_start + offset;
        let raw = template[body_start..body_end].trim().to_string();
        references.push(InputReference {
            span: cursor..body_end + 2,
            target: parse_target(&raw),
            raw,
        });
        cursor = body_end + 2;
    }
    references
}

fn parse_target(raw: &str) -> Option<InputTarget> {
    let mut parts = raw.split('.');
    let stage = parts.next()?.trim();
    let agent = parts.next()?.trim();
    let kind = parts.next()?.trim();
    if parts.next().is_some() || kind != "output" || stage.is_empty() || agent.is_empty() {
        return None;
    }
    Some(InputTarget {
        stage: stage.to_string(),
        agent: agent.to_string(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    name: String,
    #[serde(default)]
    stages: Vec<RawStage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    name: String,
    #[serde(default)]
    agents: serde_yaml::Value,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    gate: Gate,
    #[serde(default)]
    budgets: Budgets,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    recipe: Option<PathBuf>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
}

impl RawWorkflow {
    fn into_spec(self) -> Result<WorkflowSpec, WorkflowSpecError> {
        let stages = self
            .stages
            .into_iter()
            .map(RawStage::into_stage)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorkflowSpec {
            name: self.name,
            stages,
        })
    }
}

impl RawStage {
    fn into_stage(self) -> Result<Stage, WorkflowSpecError> {
        let agents = self.read_agents()?;
        Ok(Stage {
            name: self.name,
            agents,
            depends_on: self.depends_on,
            gate: self.gate,
            budgets: self.budgets,
        })
    }

    /// Fan-out : une liste d'agents nommés, ou une map dont les clés portent les
    /// noms. La map est lue via `serde_yaml::Mapping` pour garder l'ordre du
    /// document — un fan-out réordonné alphabétiquement rendrait les logs et le
    /// replay dépendants de la casse des noms.
    fn read_agents(&self) -> Result<Vec<AgentSpec>, WorkflowSpecError> {
        match &self.agents {
            serde_yaml::Value::Null => Err(WorkflowSpecError::StageWithoutAgents {
                stage: self.name.clone(),
            }),
            serde_yaml::Value::Sequence(items) if items.is_empty() => {
                Err(WorkflowSpecError::StageWithoutAgents {
                    stage: self.name.clone(),
                })
            }
            serde_yaml::Value::Mapping(entries) if entries.is_empty() => {
                Err(WorkflowSpecError::StageWithoutAgents {
                    stage: self.name.clone(),
                })
            }
            serde_yaml::Value::Sequence(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let raw = self.parse_agent(item)?;
                    let name = raw
                        .name
                        .clone()
                        .filter(|name| !name.trim().is_empty())
                        .ok_or(WorkflowSpecError::AgentNameMissing {
                            stage: self.name.clone(),
                            index,
                        })?;
                    self.build_agent(name, raw)
                })
                .collect(),
            serde_yaml::Value::Mapping(entries) => entries
                .iter()
                .map(|(key, value)| {
                    let key = key.as_str().ok_or(WorkflowSpecError::AgentsShape {
                        stage: self.name.clone(),
                    })?;
                    let raw = self.parse_agent(value)?;
                    if let Some(declared) = raw.name.as_deref() {
                        if declared != key {
                            return Err(WorkflowSpecError::AgentNameConflict {
                                stage: self.name.clone(),
                                key: key.to_string(),
                                name: declared.to_string(),
                            });
                        }
                    }
                    self.build_agent(key.to_string(), raw)
                })
                .collect(),
            _ => Err(WorkflowSpecError::AgentsShape {
                stage: self.name.clone(),
            }),
        }
    }

    fn parse_agent(&self, value: &serde_yaml::Value) -> Result<RawAgent, WorkflowSpecError> {
        serde_yaml::from_value(value.clone()).map_err(WorkflowSpecError::Yaml)
    }

    fn build_agent(&self, name: String, raw: RawAgent) -> Result<AgentSpec, WorkflowSpecError> {
        let source = match (raw.recipe, raw.prompt) {
            (Some(_), Some(_)) => {
                return Err(WorkflowSpecError::AgentSourceAmbiguous {
                    stage: self.name.clone(),
                    agent: name,
                })
            }
            (Some(recipe), None) => AgentSource::Recipe(recipe),
            (None, Some(prompt)) => AgentSource::Prompt(prompt),
            (None, None) => {
                return Err(WorkflowSpecError::AgentSourceMissing {
                    stage: self.name.clone(),
                    agent: name,
                })
            }
        };
        Ok(AgentSpec {
            name,
            source,
            model: raw.model,
            inputs: raw.inputs,
        })
    }
}
