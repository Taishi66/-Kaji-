//! Icône de mode (Octicons, Nerd Fonts v3.5.0) et son repli texte.
//!
//! Le sceau kanji dit le mode à qui lit les kanji ; l'icône le dit à qui lit
//! les cadenas. Les deux voyagent ensemble sur la barre d'état — sauf sur un
//! terminal sans Nerd Font, où `KAJI_ICONS=text` rend la barre d'avant
//! l'icône plutôt qu'un rectangle vide.

use kaji::config::KajiMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSet {
    Nerd,
    Text,
}

/// Famille verrou d'Octicons : cadenas fermé quand l'humain décide, bouclier
/// coché quand kaji juge, cadenas ouvert quand personne n'est consulté, bulle
/// quand aucun outil ne tourne.
pub fn mode_icon(set: IconSet, mode: KajiMode) -> Option<&'static str> {
    match set {
        IconSet::Text => None,
        IconSet::Nerd => Some(match mode {
            KajiMode::Approve => "\u{f456}",
            KajiMode::SmartApprove => "\u{f510}",
            KajiMode::Auto => "\u{f52a}",
            KajiMode::Chat => "\u{f442}",
        }),
    }
}

/// `KAJI_ICONS` — même contrat que `KAJI_EDIT_MODE` : une valeur inconnue ne
/// bloque jamais le lancement, `nerd` s'applique et l'appelant rend
/// l'avertissement en ligne système.
pub fn resolve(value: Option<&str>) -> (IconSet, Option<String>) {
    let Some(value) = value else {
        return (IconSet::Nerd, None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "nerd" => (IconSet::Nerd, None),
        "text" => (IconSet::Text, None),
        _ => (
            IconSet::Nerd,
            Some(format!("KAJI_ICONS invalide ({value}) — nerd appliqué")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;
    use test_case::test_case;

    #[test_case(KajiMode::Approve, "\u{f456}"; "approve_est_un_cadenas_ferme")]
    #[test_case(KajiMode::SmartApprove, "\u{f510}"; "smart_est_un_bouclier_coche")]
    #[test_case(KajiMode::Auto, "\u{f52a}"; "auto_est_un_cadenas_ouvert")]
    #[test_case(KajiMode::Chat, "\u{f442}"; "chat_est_une_bulle")]
    fn nerd_donne_une_icone_par_mode(mode: KajiMode, expected: &str) {
        assert_eq!(mode_icon(IconSet::Nerd, mode), Some(expected));
    }

    #[test]
    fn text_ne_donne_aucune_icone() {
        for mode in [
            KajiMode::Approve,
            KajiMode::SmartApprove,
            KajiMode::Auto,
            KajiMode::Chat,
        ] {
            assert_eq!(mode_icon(IconSet::Text, mode), None, "{mode:?}");
        }
    }

    /// La barre mesure son budget en cellules : une icône de deux cellules
    /// décalerait chaque troncature calculée autour d'elle.
    #[test]
    fn mode_icons_take_one_cell() {
        for mode in [
            KajiMode::Approve,
            KajiMode::SmartApprove,
            KajiMode::Auto,
            KajiMode::Chat,
        ] {
            let icon = mode_icon(IconSet::Nerd, mode).expect("une icône par mode");
            assert_eq!(Span::raw(icon).width(), 1, "{mode:?}");
        }
    }

    #[test_case(None; "absent")]
    #[test_case(Some("nerd"); "nerd")]
    #[test_case(Some("NERD"); "insensible_a_la_casse")]
    fn resolve_prend_les_icones_par_defaut(value: Option<&str>) {
        assert_eq!(resolve(value), (IconSet::Nerd, None));
    }

    #[test]
    fn resolve_accepte_le_repli_texte() {
        assert_eq!(resolve(Some("text")), (IconSet::Text, None));
    }

    #[test]
    fn resolve_avertit_sur_une_valeur_inconnue_et_garde_les_icones() {
        let (set, warning) = resolve(Some("emoji"));

        assert_eq!(set, IconSet::Nerd);
        assert_eq!(
            warning.expect("un avertissement"),
            "KAJI_ICONS invalide (emoji) — nerd appliqué"
        );
    }
}
