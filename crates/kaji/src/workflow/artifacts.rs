//! Les sorties d'agents, adressées `stage.agent`, et leur substitution dans les
//! entrées des stages descendants.
//!
//! Les spans viennent de `kaji_core::workflow::spec::input_references`, la même
//! fonction que la validation de la spec : la substitution réécrit exactement
//! les occurrences que le parser a validées, jamais un `{{…}}` que lui aurait
//! laissé passer.

use std::collections::BTreeMap;

use kaji_core::workflow::spec::input_references;

#[derive(Debug, Default, Clone)]
pub struct Artifacts {
    outputs: BTreeMap<(String, String), String>,
}

impl Artifacts {
    pub fn insert(&mut self, stage: &str, agent: &str, output: String) {
        self.outputs
            .insert((stage.to_string(), agent.to_string()), output);
    }

    pub fn get(&self, stage: &str, agent: &str) -> Option<&str> {
        self.outputs
            .get(&(stage.to_string(), agent.to_string()))
            .map(String::as_str)
    }

    /// Substitue de la fin vers le début pour que les spans des occurrences
    /// précédentes restent valides quand une sortie n'a pas la longueur de son
    /// gabarit. Une référence sans artefact reste écrite telle quelle : la
    /// validation de la spec garantit que l'ancêtre a tourné, donc un trou est
    /// un bug à voir, pas une chaîne vide à avaler.
    pub fn substitute(&self, template: &str) -> String {
        let mut rendered = template.to_string();
        for reference in input_references(template).iter().rev() {
            let Some(target) = reference.target.as_ref() else {
                continue;
            };
            let Some(output) = self.get(&target.stage, &target.agent) else {
                continue;
            };
            rendered.replace_range(reference.span.clone(), output);
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_rewrites_every_reference_of_a_template() {
        let mut artifacts = Artifacts::default();
        artifacts.insert("collecte", "scan", "42 fichiers".to_string());
        artifacts.insert("collecte", "lint", "3 alertes".to_string());

        assert_eq!(
            artifacts.substitute(
                "vu {{collecte.scan.output}} et {{collecte.lint.output}} sur {{collecte.scan.output}}"
            ),
            "vu 42 fichiers et 3 alertes sur 42 fichiers"
        );
    }

    #[test]
    fn a_reference_without_artifact_stays_visible_in_the_input() {
        let artifacts = Artifacts::default();
        assert_eq!(
            artifacts.substitute("rien de {{absent.agent.output}}"),
            "rien de {{absent.agent.output}}"
        );
    }
}
