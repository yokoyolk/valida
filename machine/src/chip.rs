use crate::Machine;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use p3_air::{Air, AirBuilder, PermutationAirBuilder, VirtualPairCol};
use p3_field::{AbstractExtensionField, AbstractField, ExtensionField, Field, Powers, PrimeField};
use p3_matrix::{dense::RowMajorMatrix, Matrix};

pub trait Chip<M: Machine> {
    /// Generate the main trace for the chip given the provided machine.
    fn generate_trace(&self, machine: &M) -> RowMajorMatrix<M::F>;

    fn local_sends(&self) -> Vec<Interaction<M::F>> {
        vec![]
    }

    fn local_receives(&self) -> Vec<Interaction<M::F>> {
        vec![]
    }

    fn global_sends(&self, _machine: &M) -> Vec<Interaction<M::F>> {
        vec![]
    }

    fn global_receives(&self, _machine: &M) -> Vec<Interaction<M::F>> {
        vec![]
    }

    fn all_interactions(&self, machine: &M) -> Vec<(Interaction<M::F>, InteractionType)> {
        let mut interactions: Vec<(Interaction<M::F>, InteractionType)> = vec![];
        interactions.extend(
            self.local_sends()
                .into_iter()
                .map(|i| (i, InteractionType::LocalSend)),
        );
        interactions.extend(
            self.local_receives()
                .into_iter()
                .map(|i| (i, InteractionType::LocalReceive)),
        );
        interactions.extend(
            self.global_sends(machine)
                .into_iter()
                .map(|i| (i, InteractionType::GlobalSend)),
        );
        interactions.extend(
            self.global_receives(machine)
                .into_iter()
                .map(|i| (i, InteractionType::GlobalReceive)),
        );
        interactions
    }

    fn interaction_map(&self, machine: &M) -> BTreeMap<BusArgument, Vec<usize>> {
        let mut map: BTreeMap<BusArgument, Vec<usize>> = BTreeMap::new();
        for (n, (interaction, _)) in self.all_interactions(machine).iter().enumerate() {
            map.entry(interaction.argument_index)
                .or_insert_with(Vec::new)
                .push(n);
        }
        map
    }
}

pub trait ValidaAir<AB: PermutationAirBuilder, M: Machine> {
    fn eval(&self, builder: &mut AB, machine: &M);
}

pub trait ValidaAirBuilder: PermutationAirBuilder {
    type PublicInput;

    fn public_input(&self) -> Option<Self::PublicInput> {
        None
    }
}

pub trait PublicInput<F> {
    fn cumulative_sum(&self) -> F;
}

pub struct Interaction<F: Field> {
    pub fields: Vec<VirtualPairCol<F>>,
    pub count: VirtualPairCol<F>,
    pub argument_index: BusArgument,
}

#[derive(Clone)]
pub enum InteractionType {
    LocalSend,
    LocalReceive,
    GlobalSend,
    GlobalReceive,
}

#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub enum BusArgument {
    Local(usize),
    Global(usize),
}

impl<F: Field> Interaction<F> {
    pub fn is_local(&self) -> bool {
        match self.argument_index {
            BusArgument::Local(_) => true,
            BusArgument::Global(_) => false,
        }
    }

    pub fn is_global(&self) -> bool {
        match self.argument_index {
            BusArgument::Local(_) => false,
            BusArgument::Global(_) => true,
        }
    }

    pub fn argument_index(&self) -> usize {
        match self.argument_index {
            BusArgument::Local(i) => i,
            BusArgument::Global(i) => i,
        }
    }
}

/// Generate the permutation trace for a chip with the provided machine.
/// This is called only after `generate_trace` has been called on all chips.
pub fn generate_permutation_trace<F: Field, M: Machine<F = F>, C: Chip<M>>(
    machine: &M,
    chip: &mut C,
    main: &RowMajorMatrix<M::F>,
    random_elements: Vec<M::EF>,
) -> RowMajorMatrix<M::EF> {
    let all_interactions = chip.all_interactions(machine);
    let (alphas_local, alphas_global) = generate_rlc_elements(chip, &random_elements);
    let betas = random_elements[2].powers();

    // Compute the reciprocal columns and build a map from bus to reciprocal column index
    //
    // Row: | q_1 | q_2 | q_3 | ... | q_n | \phi |
    // * q_i = \frac{1}{\alpha^i + \sum_j \beta^j * f_{i,j}}
    // * f_{i,j} is the jth main trace column for the ith interaction
    // * \phi is the running sum
    //
    // Note: We can optimize this by combining several reciprocal columns into one (the
    // number is subject to a target constraint degree).
    let perm_width = all_interactions.len() + 1;
    let mut perm_values = Vec::with_capacity(main.height() * perm_width);
    for main_row in main.rows() {
        let mut row = vec![M::EF::ZERO; perm_width];
        for (n, (interaction, _)) in all_interactions.iter().enumerate() {
            let alpha_i = if interaction.is_local() {
                alphas_local[interaction.argument_index()]
            } else {
                alphas_global[interaction.argument_index()]
            };
            row[n] = reduce_row(main_row, &interaction.fields, alpha_i, betas.clone());
        }
        perm_values.extend(row);
    }
    let perm_values = batch_invert(perm_values);
    let mut perm = RowMajorMatrix::new(perm_values, perm_width);

    // Compute the running sum column
    let mut phi = vec![M::EF::ZERO; perm.height() + 1];
    let map = chip.interaction_map(machine);
    for (n, (main_row, perm_row)) in main.rows().zip(perm.rows()).enumerate() {
        phi[n + 1] = phi[n];
        for (m, (interaction, interaction_type)) in all_interactions.iter().enumerate() {
            let mult = interaction.count.apply::<M::F, M::F>(&[], main_row);
            let col_idx = map[&interaction.argument_index][m];
            match interaction_type {
                InteractionType::LocalSend | InteractionType::GlobalSend => {
                    phi[n + 1] += M::EF::from_base(mult) * perm_row[col_idx];
                }
                InteractionType::LocalReceive | InteractionType::GlobalReceive => {
                    phi[n + 1] -= M::EF::from_base(mult) * perm_row[col_idx];
                }
            }
        }
    }

    for (n, row) in perm.as_view_mut().rows_mut().enumerate() {
        *row.last_mut().unwrap() = phi[n];
    }

    perm
}

pub fn eval_permutation_constraints<
    F: PrimeField,
    M: Machine<F = F>,
    C: Chip<M>,
    AB: ValidaAirBuilder<F = F, PublicInput = PI>,
    PI: PublicInput<F>,
>(
    chip: &C,
    builder: &mut AB,
    machine: &M,
) {
    let rand_elems = builder.permutation_randomness().to_vec();

    let main = builder.main();
    let main_local: &[AB::Var] = main.row(0);

    let perm = builder.permutation();
    let perm_width = perm.width();
    let perm_local: &[AB::VarEF] = perm.row(0);
    let perm_next: &[AB::VarEF] = perm.row(1);

    let phi_local = perm_local[perm_width - 1].clone();
    let phi_next = perm_next[perm_width - 1].clone();

    let cumulative_sum = builder.public_input().unwrap().cumulative_sum();

    let all_interactions = chip.all_interactions(machine);
    let map = chip.interaction_map(machine);

    let (alphas_local, alphas_global) = generate_rlc_elements(chip, &rand_elems);
    let betas = rand_elems[2].powers();

    let lhs = phi_next - phi_local.clone();
    let mut rhs = AB::ExprEF::from_base(AB::F::ZERO);
    for (m, (interaction, interaction_type)) in all_interactions.iter().enumerate() {
        let col_idx = map[&interaction.argument_index][m];

        // Reciprocal constraints
        let mut rlc = AB::ExprEF::from_base(AB::F::ZERO);
        for (field, beta) in interaction.fields.iter().zip(betas.clone()) {
            let elem = field.apply::<AB::Expr, AB::Var>(&[], main_local);
            rlc += AB::ExprEF::from(beta) * elem;
        }
        if interaction.is_local() {
            rlc = rlc + alphas_local[interaction.argument_index()];
        } else {
            rlc = rlc + alphas_global[interaction.argument_index()];
        }
        builder.assert_eq_ext(rlc, perm_local[col_idx].clone().into());

        // Build the RHS of the permutation constraint
        let mult = interaction
            .count
            .apply::<AB::Expr, AB::Var>(&[], main_local);
        match interaction_type {
            InteractionType::LocalSend | InteractionType::GlobalSend => {
                rhs += AB::ExprEF::from(mult) * perm_local[col_idx];
            }
            InteractionType::LocalReceive | InteractionType::GlobalReceive => {
                rhs -= AB::ExprEF::from(mult) * perm_local[col_idx];
            }
        }
    }

    // Running sum constraints
    builder.when_transition().assert_eq_ext(lhs, rhs);
    builder.when_first_row().assert_zero_ext(phi_local);
    builder
        .when_last_row()
        .assert_eq_ext(perm_local[0].clone(), AB::ExprEF::from_base(cumulative_sum));
}

fn generate_rlc_elements<
    C: Chip<M>,
    M: Machine,
    F: AbstractField,
    EF: AbstractExtensionField<F>,
>(
    chip: &C,
    random_elements: &[EF],
) -> (Vec<EF>, Vec<EF>) {
    let alphas_local = random_elements[0]
        .powers()
        .skip(1)
        .take(
            chip.local_sends()
                .iter()
                .map(|interaction| interaction.argument_index())
                .max()
                .unwrap(),
        )
        .collect::<Vec<_>>();

    let alphas_global = random_elements[1]
        .powers()
        .skip(1)
        .take(
            chip.local_sends()
                .iter()
                .map(|interaction| interaction.argument_index())
                .max()
                .unwrap(),
        )
        .collect::<Vec<_>>();

    (alphas_local, alphas_global)
}

// TODO: Use Var and Expr type bounds in place of concrete fields so that
// this function can be used in `eval_permutation_constraints`.
fn reduce_row<F: Field, EF: ExtensionField<F>>(
    row: &[F],
    fields: &[VirtualPairCol<F>],
    alpha: EF,
    betas: Powers<EF>,
) -> EF {
    let mut rlc = EF::ZERO;
    for (columns, beta) in fields.iter().zip(betas) {
        rlc += beta * columns.apply::<F, F>(&[], row)
    }
    rlc += alpha;
    rlc
}

pub fn batch_invert<F: Field>(values: Vec<F>) -> Vec<F> {
    let mut res = vec![F::ZERO; values.len()];
    let mut prod = F::ONE;
    for (n, value) in values.iter().cloned().enumerate() {
        res[n] = prod;
        prod *= value;
    }
    let mut inv = prod.inverse();
    for (n, value) in values.iter().cloned().rev().enumerate().rev() {
        res[n] *= inv;
        inv *= value;
    }
    res
}

#[macro_export]
macro_rules! instructions {
    ($($t:ident),*) => {
        $(
            #[derive(Default)]
            pub struct $t {}
        )*
    }
}
