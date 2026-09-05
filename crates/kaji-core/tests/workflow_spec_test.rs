use kaji_core::workflow::spec::{
    AgentSource, Budgets, Gate, InputTarget, WorkflowSpec, WorkflowSpecError,
};

const MINIMAL: &str = r#"
name: revue
stages:
  - name: analyse
    agents:
      - name: lecteur
        prompt: lis le diff
"#;

#[test]
fn a_minimal_spec_parses_into_one_stage_and_one_agent() {
    let spec = WorkflowSpec::from_yaml(MINIMAL).expect("spec valide");

    assert_eq!(spec.name, "revue");
    assert_eq!(spec.stages.len(), 1);
    let stage = &spec.stages[0];
    assert_eq!(stage.name, "analyse");
    assert_eq!(stage.agents.len(), 1);
    assert_eq!(stage.agents[0].name, "lecteur");
    assert_eq!(
        stage.agents[0].source,
        AgentSource::Prompt("lis le diff".to_string())
    );
}

#[test]
fn an_omitted_gate_budget_or_dependency_takes_its_default() {
    let spec = WorkflowSpec::from_yaml(MINIMAL).expect("spec valide");
    let stage = &spec.stages[0];

    assert_eq!(stage.gate, Gate::Auto);
    assert_eq!(stage.depends_on, Vec::<String>::new());
    assert_eq!(stage.budgets, Budgets::default());
    assert_eq!(stage.agents[0].model, None);
    assert!(stage.agents[0].inputs.is_empty());
}

#[test]
fn a_list_of_agents_fans_out_in_declaration_order() {
    let yaml = r#"
name: revue
stages:
  - name: analyse
    agents:
      - name: zeta
        prompt: un
      - name: alpha
        recipe: recipes/alpha.yaml
"#;

    let spec = WorkflowSpec::from_yaml(yaml).expect("spec valide");
    let agents = &spec.stages[0].agents;

    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].name, "zeta");
    assert_eq!(agents[1].name, "alpha");
    assert_eq!(
        agents[1].source,
        AgentSource::Recipe("recipes/alpha.yaml".into())
    );
}

#[test]
fn a_map_of_agents_fans_out_with_its_keys_as_names_in_yaml_order() {
    let yaml = r#"
name: revue
stages:
  - name: analyse
    agents:
      zeta:
        prompt: un
      alpha:
        prompt: deux
"#;

    let spec = WorkflowSpec::from_yaml(yaml).expect("spec valide");
    let agents = &spec.stages[0].agents;

    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].name, "zeta");
    assert_eq!(agents[1].name, "alpha");
}

#[test]
fn a_full_stage_carries_its_gate_budgets_dependencies_and_templated_inputs() {
    let yaml = r#"
name: revue
stages:
  - name: analyse
    agents:
      - name: lecteur
        prompt: lis
  - name: synthese
    depends_on: [analyse]
    gate: approve
    budgets:
      max_tokens: 10000
      max_duration_s: 600
    agents:
      - name: redacteur
        recipe: recipes/redacteur.yaml
        model: opus
        inputs:
          rapport: "{{analyse.lecteur.output}}"
"#;

    let spec = WorkflowSpec::from_yaml(yaml).expect("spec valide");
    let stage = &spec.stages[1];

    assert_eq!(stage.depends_on, vec!["analyse".to_string()]);
    assert_eq!(stage.gate, Gate::Approve);
    assert_eq!(stage.budgets.max_tokens, Some(10_000));
    assert_eq!(stage.budgets.max_duration_s, Some(600));
    assert_eq!(stage.agents[0].model.as_deref(), Some("opus"));
    assert_eq!(
        stage.agents[0].inputs.get("rapport").map(String::as_str),
        Some("{{analyse.lecteur.output}}")
    );
}

#[test]
fn a_dependency_declared_before_its_provider_stays_acyclic_and_valid() {
    let yaml = r#"
name: revue
stages:
  - name: aval
    depends_on: [amont]
    agents:
      - name: un
        prompt: un
  - name: amont
    agents:
      - name: deux
        prompt: deux
"#;

    assert!(WorkflowSpec::from_yaml(yaml).is_ok());
}

#[test]
fn a_diamond_of_dependencies_is_a_valid_dag() {
    let yaml = r#"
name: revue
stages:
  - name: source
    agents:
      - name: u
        prompt: p
  - name: gauche
    depends_on: [source]
    agents:
      - name: v
        prompt: p
  - name: droite
    depends_on: [source]
    agents:
      - name: w
        prompt: p
  - name: fusion
    depends_on: [gauche, droite]
    agents:
      - name: x
        prompt: p
        inputs:
          g: "{{gauche.v.output}}"
          d: "{{droite.w.output}}"
"#;

    assert!(WorkflowSpec::from_yaml(yaml).is_ok());
}

#[test]
fn a_three_stage_cycle_names_the_whole_loop() {
    let yaml = r#"
name: revue
stages:
  - name: a
    depends_on: [c]
    agents:
      - name: u
        prompt: p
  - name: b
    depends_on: [a]
    agents:
      - name: v
        prompt: p
  - name: c
    depends_on: [b]
    agents:
      - name: w
        prompt: p
"#;

    match WorkflowSpec::from_yaml(yaml) {
        Err(WorkflowSpecError::DependencyCycle { stages }) => {
            assert_eq!(stages.len(), 3);
            assert!(stages.contains(&"a".to_string()));
            assert!(stages.contains(&"b".to_string()));
            assert!(stages.contains(&"c".to_string()));
        }
        other => panic!("cycle attendu, obtenu {other:?}"),
    }
}

#[test]
fn input_references_carry_their_span_and_target() {
    let refs = kaji_core::workflow::spec::input_references("avant {{ a.b.output }} apres");

    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].span, 6..22);
    assert_eq!(
        refs[0].target,
        Some(InputTarget {
            stage: "a".to_string(),
            agent: "b".to_string()
        })
    );
}

#[test]
fn a_reference_that_is_not_stage_agent_output_has_no_target() {
    let refs = kaji_core::workflow::spec::input_references("{{a.b}} {{a.b.c.d}} {{a.b.result}}");

    assert_eq!(refs.len(), 3);
    assert!(refs.iter().all(|reference| reference.target.is_none()));
}

#[test]
fn validate_reruns_the_checks_on_a_parsed_spec() {
    let mut spec = WorkflowSpec::from_yaml(MINIMAL).expect("spec valide");
    assert!(spec.validate().is_ok());

    spec.stages[0].budgets.max_tokens = Some(0);

    assert!(matches!(
        spec.validate(),
        Err(WorkflowSpecError::NonPositiveBudget { .. })
    ));
}

#[test]
fn every_rejection_names_its_own_cause() {
    type Check = fn(&WorkflowSpecError) -> bool;
    let cases: Vec<(&str, &str, Check)> = vec![
        (
            "yaml malformé",
            "name: revue\nstages: [ - oups",
            |e| matches!(e, WorkflowSpecError::Yaml(_)),
        ),
        (
            "champ inconnu",
            "name: revue\nstages:\n  - name: a\n    dependson: []\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::Yaml(_)),
        ),
        (
            "nom de workflow vide",
            "name: \"\"\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::EmptyWorkflowName),
        ),
        (
            "aucun stage",
            "name: revue\nstages: []\n",
            |e| matches!(e, WorkflowSpecError::NoStages),
        ),
        (
            "nom de stage vide",
            "name: revue\nstages:\n  - name: \"\"\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::EmptyStageName { index: 0 }),
        ),
        (
            "stage sans agents",
            "name: revue\nstages:\n  - name: a\n    agents: []\n",
            |e| matches!(e, WorkflowSpecError::StageWithoutAgents { stage } if stage == "a"),
        ),
        (
            "stage sans clé agents",
            "name: revue\nstages:\n  - name: a\n",
            |e| matches!(e, WorkflowSpecError::StageWithoutAgents { stage } if stage == "a"),
        ),
        (
            "agents ni liste ni map",
            "name: revue\nstages:\n  - name: a\n    agents: lecteur\n",
            |e| matches!(e, WorkflowSpecError::AgentsShape { stage } if stage == "a"),
        ),
        (
            "stages en double",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n  - name: a\n    agents:\n      - name: v\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::DuplicateStageName { name } if name == "a"),
        ),
        (
            "agents en double",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n      - name: u\n        prompt: q\n",
            |e| matches!(e, WorkflowSpecError::DuplicateAgentName { stage, name } if stage == "a" && name == "u"),
        ),
        (
            "agent de liste sans nom",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - prompt: p\n",
            |e| matches!(e, WorkflowSpecError::AgentNameMissing { stage, index: 0 } if stage == "a"),
        ),
        (
            "agent de map au nom contradictoire",
            "name: revue\nstages:\n  - name: a\n    agents:\n      u:\n        name: v\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::AgentNameConflict { stage, key, name } if stage == "a" && key == "u" && name == "v"),
        ),
        (
            "agent sans recipe ni prompt",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        model: opus\n",
            |e| matches!(e, WorkflowSpecError::AgentSourceMissing { stage, agent } if stage == "a" && agent == "u"),
        ),
        (
            "agent avec recipe et prompt",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n        recipe: r.yaml\n",
            |e| matches!(e, WorkflowSpecError::AgentSourceAmbiguous { stage, agent } if stage == "a" && agent == "u"),
        ),
        (
            "source d'agent vide",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: \"  \"\n",
            |e| matches!(e, WorkflowSpecError::EmptyAgentSource { stage, agent } if stage == "a" && agent == "u"),
        ),
        (
            "dépendance inconnue",
            "name: revue\nstages:\n  - name: a\n    depends_on: [absent]\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::UnknownDependency { stage, depends_on } if stage == "a" && depends_on == "absent"),
        ),
        (
            "dépendance sur soi",
            "name: revue\nstages:\n  - name: a\n    depends_on: [a]\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::SelfDependency { stage } if stage == "a"),
        ),
        (
            "cycle",
            "name: revue\nstages:\n  - name: a\n    depends_on: [b]\n    agents:\n      - name: u\n        prompt: p\n  - name: b\n    depends_on: [a]\n    agents:\n      - name: v\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::DependencyCycle { stages } if stages.len() == 2),
        ),
        (
            "référence d'entrée malformée",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n  - name: b\n    agents:\n      - name: v\n        prompt: p\n        inputs:\n          x: \"{{a.u}}\"\n",
            |e| matches!(e, WorkflowSpecError::InputReferenceMalformed { stage, agent, input, .. } if stage == "b" && agent == "v" && input == "x"),
        ),
        (
            "référence vers un stage inconnu",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n  - name: b\n    agents:\n      - name: v\n        prompt: p\n        inputs:\n          x: \"{{absent.u.output}}\"\n",
            |e| matches!(e, WorkflowSpecError::UnknownInputStage { referenced, .. } if referenced == "absent"),
        ),
        (
            "référence vers un agent inconnu",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n  - name: b\n    agents:\n      - name: v\n        prompt: p\n        inputs:\n          x: \"{{a.absent.output}}\"\n",
            |e| matches!(e, WorkflowSpecError::UnknownInputAgent { referenced_agent, .. } if referenced_agent == "absent"),
        ),
        (
            "référence vers un stage postérieur",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n        inputs:\n          x: \"{{b.v.output}}\"\n  - name: b\n    agents:\n      - name: v\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::InputReferenceNotEarlier { stage, referenced_stage, .. } if stage == "a" && referenced_stage == "b"),
        ),
        (
            "référence vers son propre stage",
            "name: revue\nstages:\n  - name: a\n    agents:\n      - name: u\n        prompt: p\n      - name: w\n        prompt: p\n        inputs:\n          x: \"{{a.u.output}}\"\n",
            |e| matches!(e, WorkflowSpecError::InputReferenceNotEarlier { stage, referenced_stage, .. } if stage == "a" && referenced_stage == "a"),
        ),
        (
            "budget négatif",
            "name: revue\nstages:\n  - name: a\n    budgets:\n      max_tokens: -1\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::NonPositiveBudget { stage, field, value: -1 } if stage == "a" && *field == "max_tokens"),
        ),
        (
            "budget nul",
            "name: revue\nstages:\n  - name: a\n    budgets:\n      max_duration_s: 0\n    agents:\n      - name: u\n        prompt: p\n",
            |e| matches!(e, WorkflowSpecError::NonPositiveBudget { stage, field, value: 0 } if stage == "a" && *field == "max_duration_s"),
        ),
    ];

    for (label, yaml, check) in cases {
        match WorkflowSpec::from_yaml(yaml) {
            Ok(_) => panic!("{label} : la spec aurait dû être rejetée"),
            Err(error) => {
                assert!(check(&error), "{label} : cause inattendue — {error:?}");
                assert!(
                    !error.to_string().is_empty(),
                    "{label} : erreur sans message"
                );
            }
        }
    }
}

#[test]
fn a_rejection_message_names_the_stage_at_fault() {
    let yaml = "name: revue\nstages:\n  - name: analyse\n    depends_on: [absent]\n    agents:\n      - name: u\n        prompt: p\n";

    let message = WorkflowSpec::from_yaml(yaml).unwrap_err().to_string();

    assert!(message.contains("analyse"), "message : {message}");
    assert!(message.contains("absent"), "message : {message}");
}
