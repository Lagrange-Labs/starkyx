use core::marker::PhantomData;

use itertools::Itertools;
use plonky2::field::extension::Extendable;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::generator::{GeneratedValues, SimpleGenerator};
use plonky2::iop::target::Target;
use plonky2::iop::witness::{PartitionWitness, Witness, WitnessWrite};
use plonky2::plonk::circuit_builder::CircuitBuilder;
use plonky2::plonk::circuit_data::CommonCircuitData;
use plonky2::util::serialization::Buffer;

use super::{SHA256Gadget, SHA256PublicData, INITIAL_HASH, ROUND_CONSTANTS};
use crate::chip::register::Register;
use crate::chip::trace::generator::ArithmeticGenerator;
use crate::chip::uint::bytes::lookup_table::table::ByteLookupTable;
use crate::chip::uint::operations::instruction::U32Instruction;
use crate::chip::uint::register::U32Register;
use crate::chip::uint::util::u32_to_le_field_bytes;
use crate::chip::AirParameters;
use crate::math::prelude::{CubicParameters, *};

#[derive(Debug, Clone, Copy)]
pub struct SHA256AirParameters<F, E>(pub PhantomData<(F, E)>);

pub type U32Target = <U32Register as Register>::Value<Target>;

pub const SHA256_COLUMNS: usize = 551 + 927;

#[derive(Debug, Clone)]
pub struct MessageChunks {
    pub values: Vec<u8>,
    pub chunk_sizes: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct SHA256Generator<F: PrimeField64, E: CubicParameters<F>> {
    pub gadget: SHA256Gadget,
    pub table: ByteLookupTable,
    pub padded_messages: Vec<Target>,
    pub chunk_sizes: Vec<usize>,
    pub trace_generator: ArithmeticGenerator<SHA256AirParameters<F, E>>,
    pub pub_values_target: SHA256PublicData<Target>,
}

impl<F: PrimeField64, E: CubicParameters<F>> AirParameters for SHA256AirParameters<F, E> {
    type Field = F;
    type CubicParams = E;

    type Instruction = U32Instruction;

    const NUM_FREE_COLUMNS: usize = 551;
    const EXTENDED_COLUMNS: usize = 927;
    const NUM_ARITHMETIC_COLUMNS: usize = 0;

    fn num_rows_bits() -> usize {
        16
    }
}

impl<F: RichField + Extendable<D>, E: CubicParameters<F>, const D: usize> SimpleGenerator<F, D>
    for SHA256Generator<F, E>
{
    fn id(&self) -> String {
        "SHA256 generator".to_string()
    }

    fn serialize(
        &self,
        _dst: &mut Vec<u8>,
        _common_data: &CommonCircuitData<F, D>,
    ) -> plonky2::util::serialization::IoResult<()> {
        unimplemented!("SHA256Generator::serialize")
    }

    fn deserialize(
        _src: &mut Buffer,
        _common_data: &CommonCircuitData<F, D>,
    ) -> plonky2::util::serialization::IoResult<Self>
    where
        Self: Sized,
    {
        unimplemented!("SHA256Generator::deserialize")
    }

    fn dependencies(&self) -> Vec<Target> {
        self.padded_messages.clone()
    }

    fn run_once(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) {
        let padded_messages = self
            .padded_messages
            .iter()
            .map(|x| witness.get_target(*x).as_canonical_u64() as u8)
            .collect::<Vec<_>>();
        assert_eq!(padded_messages.len(), 1024 * 64);

        let message_chunks = self.chunk_sizes.iter().scan(0, |idx, size| {
            let chunk = padded_messages[*idx..*idx + 64 * size].to_vec();
            *idx += 64 * size;
            Some(chunk)
        });

        // Write trace values
        let writer = self.trace_generator.new_writer();
        self.table.write_table_entries(&writer);
        let sha_public_values = self.gadget.write(message_chunks, &writer);
        for i in 0..SHA256AirParameters::<F, E>::num_rows() {
            writer.write_row_instructions(&self.trace_generator.air_data, i);
        }
        self.table.write_multiplicities(&writer);

        // Fill sha public values into the output buffer
        self.pub_values_target
            .set_targets(sha_public_values, out_buffer);
    }
}

impl SHA256PublicData<Target> {
    pub fn add_virtual<F: RichField + Extendable<D>, const D: usize>(
        builder: &mut CircuitBuilder<F, D>,
        digests: &[Target],
        chunk_sizes: &[usize],
    ) -> Self {
        let public_w_targets = (0..16 * 1024)
            .map(|_| builder.add_virtual_target_arr::<4>())
            .collect::<Vec<_>>();

        // let end_bits_targets = builder.add_virtual_targets(1024);
        let mut end_bits_targets = Vec::new();
        let mut hash_state_targets = Vec::new();

        for (digest, chunk_size) in digests.chunks_exact(32).zip_eq(chunk_sizes.iter()) {
            end_bits_targets.extend((0..(chunk_size - 1)).map(|_| builder.zero()));
            end_bits_targets.push(builder.one());

            hash_state_targets
                .extend((0..8 * (chunk_size - 1)).map(|_| builder.add_virtual_target_arr::<4>()));

            // Convert digest to little endian u32 chunks
            let u32_digest = digest.chunks_exact(4).map(|arr| {
                let mut array: [Target; 4] = arr.try_into().unwrap();
                array.reverse();
                array
            });
            hash_state_targets.extend(u32_digest);
        }

        SHA256PublicData {
            public_w: public_w_targets,
            hash_state: hash_state_targets,
            end_bits: end_bits_targets,
        }
    }

    pub fn set_targets<F: RichField>(
        &self,
        values: SHA256PublicData<F>,
        out_buffer: &mut GeneratedValues<F>,
    ) {
        for (pub_w_target, pub_w_value) in self.public_w.iter().zip_eq(values.public_w.iter()) {
            out_buffer.set_target_arr(pub_w_target, pub_w_value);
        }
        for (hash_target, hash_value) in self.hash_state.iter().zip_eq(values.hash_state.iter()) {
            out_buffer.set_target_arr(hash_target, hash_value);
        }
    }

    pub fn public_input_targets<F: RichField + Extendable<D>, const D: usize>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
    ) -> Vec<Target> {
        self.public_w
            .iter()
            .flatten()
            .copied()
            .chain(
                INITIAL_HASH
                    .map(|value| u32_to_le_field_bytes(value).map(|x| builder.constant(x)))
                    .into_iter()
                    .flatten(),
            )
            .chain(
                ROUND_CONSTANTS
                    .map(|value| u32_to_le_field_bytes(value).map(|x| builder.constant(x)))
                    .into_iter()
                    .flatten(),
            )
            .chain(self.hash_state.iter().flatten().copied())
            .chain(self.end_bits.iter().copied())
            .collect()
    }
}