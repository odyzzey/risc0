// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::vec::Vec;

use log::debug;
use rand::RngCore;
use risc0_core::field::{Elem, ExtElem};

use crate::{
    core::{log2_ceil, sha::Sha256},
    hal::{Buffer, Hal, ProverHash},
    prove::{merkle::MerkleTreeProver, write_iop::WriteIOP},
    FRI_FOLD, FRI_MIN_DEGREE, INV_RATE, QUERIES,
};

struct ProveRoundInfo<H, PH> where H: Hal, PH: ProverHash<H> {
    //phantom: PhantomData<PH>,
    domain: usize,
    coeffs: H::BufferElem,
    merkle: MerkleTreeProver<H, PH>,
}

impl<H, PH> ProveRoundInfo<H, PH> where H: Hal, PH: ProverHash<H> {
    /// Computes a round of the folding protocol. Takes in the coefficients of
    /// the current polynomial, and interacts with the IOP verifier to
    /// produce the evaluations of the polynomial, the merkle tree
    /// committing to the evaluation, and the coefficients of the folded
    /// polynomial.
    pub fn new<S: Sha256>(hal: &H, iop: &mut WriteIOP<S>, coeffs: &H::BufferElem) -> Self {
        debug!("Doing FRI folding");
        let ext_size = H::ExtElem::EXT_SIZE;
        // Get the number of coefficients of the polynomial over the extension field.
        let size = coeffs.size() / ext_size;
        // Get a larger domain to interpolate over.
        let domain = size * INV_RATE;
        // Allocate space in which to put the interpolated values.
        let evaluated = hal.alloc_elem("evaluated", domain * ext_size);
        // Put in the coefficients, padding out with zeros so that we are left with the
        // same polynomial represented by a larger coefficient list
        hal.batch_expand(&evaluated, coeffs, ext_size);
        // Evaluate the NTT in-place, filling the buffer with the evaluations of the
        // polynomial.
        hal.batch_evaluate_ntt(&evaluated, ext_size, log2_ceil(INV_RATE));
        // Compute a Merkle tree committing to the polynomial evaluations.
        let merkle = MerkleTreeProver::new(
            hal,
            &evaluated,
            domain / FRI_FOLD,
            FRI_FOLD * ext_size,
            QUERIES,
        );
        // Send the merkle tree (as a commitment) to the virtual IOP verifier
        merkle.commit(iop);
        // Retrieve from the IOP verifier a random value to mix the polynomial slices.
        let fold_mix = H::ExtElem::random(&mut iop.rng);
        // Create a buffer to hold the mixture of slices.
        let out_coeffs = hal.alloc_elem("out_coeffs", size / FRI_FOLD * ext_size);
        // Compute the folded polynomial
        hal.fri_fold(&out_coeffs, coeffs, &fold_mix);
        ProveRoundInfo {
            domain,
            coeffs: out_coeffs,
            merkle,
        }
    }

    pub fn prove_query<S: Sha256>(&mut self, iop: &mut WriteIOP<S>, pos: &mut usize) {
        // Compute which group we are in
        let group = *pos % (self.domain / FRI_FOLD);
        // Generate the proof
        self.merkle.prove(iop, group);
        // Update pos
        *pos = group;
    }
}

#[tracing::instrument(skip_all)]
pub fn fri_prove<H: Hal, PH, S: Sha256, F>(
    hal: &H,
    iop: &mut WriteIOP<S>,
    coeffs: &H::BufferElem,
    mut f: F,
) where
    PH: ProverHash<H>,
    F: FnMut(&mut WriteIOP<S>, usize),
{
    let ext_size = H::ExtElem::EXT_SIZE;
    let orig_domain = coeffs.size() / ext_size * INV_RATE;
    let mut rounds = Vec::new();
    let mut coeffs = coeffs.clone();
    while coeffs.size() / ext_size > FRI_MIN_DEGREE {
        let round : ProveRoundInfo<H, PH> = ProveRoundInfo::new(hal, iop, &coeffs);
        coeffs = round.coeffs.clone();
        rounds.push(round);
    }
    // Put the final coefficients into natural order
    let final_coeffs = hal.alloc_elem("final_coeffs", coeffs.size());
    hal.eltwise_copy_elem(&final_coeffs, &coeffs);
    hal.batch_bit_reverse(&final_coeffs, ext_size);
    // Dump final polynomial + commit
    final_coeffs.view(|view| {
        iop.write_field_elem_slice::<H::Elem>(view);
        let digest = S::hash_raw_pod_slice(view);
        iop.commit(&digest);
    });
    // Do queries
    debug!("Doing Queries");
    for _ in 0..QUERIES {
        // Get a 'random' index.
        let rng = iop.rng.next_u32() as usize;
        let mut pos = rng % orig_domain;
        // Do the 'inner' proof for this index
        f(iop, pos);
        // Write the per-round proofs
        for round in rounds.iter_mut() {
            round.prove_query(iop, &mut pos);
        }
    }
}
