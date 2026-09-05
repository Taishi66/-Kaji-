//! Burn rate et projection de fin de mois par régression linéaire simple sur
//! les jours écoulés du mois courant.

use serde::Serialize;

/// Projection de dépense sur le mois courant. `month_end` n'est jamais
/// inférieur à `spent` : une pente négative (dépense qui ralentit) projette
/// au pire le déjà-dépensé, pas un remboursement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Projection {
    pub elapsed_days: u32,
    pub days_in_month: u32,
    pub spent: f64,
    /// Pente de la régression, en USD/jour.
    pub daily_rate: f64,
    pub month_end: f64,
}

impl Projection {
    pub fn empty(days_in_month: u32) -> Self {
        Projection {
            elapsed_days: 0,
            days_in_month,
            spent: 0.0,
            daily_rate: 0.0,
            month_end: 0.0,
        }
    }
}

/// Régression des moindres carrés sur la dépense **cumulée** jour par jour
/// (`x` = rang du jour 1-basé, `y` = cumul à la fin de ce jour), extrapolée
/// au dernier jour du mois.
///
/// Le cumul plutôt que la dépense quotidienne : c'est la grandeur que la
/// projection doit prolonger, et elle lisse les jours creux sans donner à un
/// seul pic le poids qu'il aurait sur une pente de série brute.
pub fn project_month_end(daily_costs: &[f64], days_in_month: u32) -> Projection {
    let n = daily_costs.len();
    if n == 0 || days_in_month == 0 {
        return Projection::empty(days_in_month);
    }

    let mut cumulative = Vec::with_capacity(n);
    let mut running = 0.0;
    for cost in daily_costs {
        running += cost;
        cumulative.push(running);
    }
    let spent = running;
    let elapsed_days = n as u32;

    let mean_rate = spent / n as f64;
    let horizon = f64::from(days_in_month);

    if n == 1 {
        return Projection {
            elapsed_days,
            days_in_month,
            spent,
            daily_rate: mean_rate,
            month_end: (mean_rate * horizon).max(spent),
        };
    }

    let n_f = n as f64;
    let mean_x = (n_f + 1.0) / 2.0;
    let mean_y = cumulative.iter().sum::<f64>() / n_f;

    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (index, y) in cumulative.iter().enumerate() {
        let dx = (index as f64 + 1.0) - mean_x;
        numerator += dx * (y - mean_y);
        denominator += dx * dx;
    }

    let slope = if denominator.abs() < f64::EPSILON {
        mean_rate
    } else {
        numerator / denominator
    };

    if slope <= 0.0 {
        return Projection {
            elapsed_days,
            days_in_month,
            spent,
            daily_rate: mean_rate,
            month_end: (mean_rate * horizon).max(spent),
        };
    }

    let intercept = mean_y - slope * mean_x;
    let month_end = (intercept + slope * horizon).max(spent);

    Projection {
        elapsed_days,
        days_in_month,
        spent,
        daily_rate: slope,
        month_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn constant_daily_spend_projects_the_exact_monthly_total() {
        let p = project_month_end(&[2.0; 10], 30);
        assert_eq!(p.elapsed_days, 10);
        assert!(close(p.spent, 20.0), "spent = {}", p.spent);
        assert!(close(p.daily_rate, 2.0), "rate = {}", p.daily_rate);
        assert!(close(p.month_end, 60.0), "month_end = {}", p.month_end);
    }

    #[test]
    fn accelerating_spend_projects_above_the_flat_extrapolation() {
        let p = project_month_end(&[1.0, 2.0, 3.0, 4.0, 5.0], 30);
        assert!(close(p.spent, 15.0));
        let flat = p.spent / 5.0 * 30.0;
        assert!(
            p.month_end > flat,
            "projection {} devrait dépasser l'extrapolation plate {flat}",
            p.month_end
        );
    }

    #[test]
    fn empty_series_projects_zero() {
        let p = project_month_end(&[], 31);
        assert_eq!(p.elapsed_days, 0);
        assert!(close(p.spent, 0.0));
        assert!(close(p.month_end, 0.0));
        assert_eq!(p.days_in_month, 31);
    }

    #[test]
    fn single_day_extrapolates_that_day_over_the_month() {
        let p = project_month_end(&[3.0], 30);
        assert_eq!(p.elapsed_days, 1);
        assert!(close(p.daily_rate, 3.0));
        assert!(close(p.month_end, 90.0));
    }

    #[test]
    fn projection_never_falls_below_what_is_already_spent() {
        // Dépense concentrée au début puis plus rien : la pente du cumul
        // reste positive mais faible ; le mois ne peut pas finir en dessous.
        let p = project_month_end(&[100.0, 0.0, 0.0, 0.0, 0.0], 31);
        assert!(close(p.spent, 100.0));
        assert!(
            p.month_end >= p.spent,
            "month_end = {} < spent = {}",
            p.month_end,
            p.spent
        );
    }

    #[test]
    fn all_zero_days_project_zero_without_dividing_by_zero() {
        let p = project_month_end(&[0.0, 0.0, 0.0], 30);
        assert!(close(p.daily_rate, 0.0));
        assert!(close(p.month_end, 0.0));
    }
}
