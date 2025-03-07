use crate::{
    evm_circuit::{
        execution::ExecutionGadget,
        param::{N_BYTES_MEMORY_ADDRESS, N_BYTES_MEMORY_WORD_SIZE, N_BYTES_U64},
        step::ExecutionState,
        util::{
            common_gadget::{CommonReturnDataCopyGadget, SameContextGadget},
            constraint_builder::{
                ConstrainBuilderCommon, EVMConstraintBuilder, StepStateTransition,
                Transition::{Delta, To},
            },
            from_bytes,
            memory_gadget::{
                CommonMemoryAddressGadget, MemoryAddressGadget, MemoryCopierGasGadget,
                MemoryExpansionGadget,
            },
            CachedRegion, Cell, RandomLinearCombination,
        },
        witness::{Block, Call, ExecStep, Transaction},
    },
    table::CallContextFieldTag,
    util::{Expr, Field},
};
use bus_mapping::{circuit_input_builder::CopyDataType, evm::OpcodeId};
use eth_types::evm_types::GasCost;
use gadgets::util::not;
use gadgets::ToScalar;
use halo2_proofs::{circuit::Value, plonk::Error};

#[derive(Clone, Debug)]
pub(crate) struct ReturnDataCopyGadget<F> {
    same_context: SameContextGadget<F>,
    /// Holds the last_called_id for copy table.
    last_callee_id: Cell<F>,
    /// Holds the memory address for return data from where we read.
    return_data_offset: Cell<F>,
    /// Holds the size of the return data.
    return_data_size: Cell<F>,
    /// The data is copied to memory. To verify this
    /// copy operation we need the MemoryAddressGadget.
    dst_memory_addr: MemoryAddressGadget<F>,
    /// check if overflow
    check_overflow_gadget: CommonReturnDataCopyGadget<F>,
    /// Opcode RETURNDATACOPY has a dynamic gas cost:
    /// gas_code = static_gas * minimum_word_size + memory_expansion_cost
    memory_expansion: MemoryExpansionGadget<F, 1, N_BYTES_MEMORY_WORD_SIZE>,
    /// Opcode RETURNDATAECOPY needs to copy data into memory. We account for
    /// the copying costs using the memory copier gas gadget.
    memory_copier_gas: MemoryCopierGasGadget<F, { GasCost::COPY }>,
    /// RW inverse counter from the copy table at the start of related copy
    /// steps.
    copy_rwc_inc: Cell<F>,
}

impl<F: Field> ExecutionGadget<F> for ReturnDataCopyGadget<F> {
    const NAME: &'static str = "RETURNDATACOPY";

    const EXECUTION_STATE: ExecutionState = ExecutionState::RETURNDATACOPY;

    fn configure(cb: &mut EVMConstraintBuilder<F>) -> Self {
        let opcode = cb.query_cell();

        let dest_offset = cb.query_cell_phase2();
        let return_data_size: Cell<F> = cb.query_cell();

        let size: RandomLinearCombination<F, N_BYTES_MEMORY_ADDRESS> = cb.query_word_rlc();
        // enusre no other out of bound errors occur, otherwise go to `ErrorReturnDataOutOfBound`
        // state
        let check_overflow_gadget =
            CommonReturnDataCopyGadget::construct(cb, return_data_size.expr(), false.expr());
        // in normal case, size = CommonReturnDataCopyGadget::size
        cb.require_equal(
            "size = CommonReturnDataCopyGadget::size",
            size.expr(),
            check_overflow_gadget.size().expr(),
        );
        // 1. Pop dest_offset, offset, length from stack
        cb.stack_pop(dest_offset.expr());
        cb.stack_pop(check_overflow_gadget.data_offset().expr());
        cb.stack_pop(size.expr());

        // 2. Add lookup constraint in the call context for the returndatacopy field.
        let last_callee_id = cb.query_cell();
        let return_data_offset = cb.query_cell();
        cb.call_context_lookup(
            false.expr(),
            None,
            CallContextFieldTag::LastCalleeId,
            last_callee_id.expr(),
        );
        cb.call_context_lookup(
            false.expr(),
            None,
            CallContextFieldTag::LastCalleeReturnDataOffset,
            return_data_offset.expr(),
        );
        cb.call_context_lookup(
            false.expr(),
            None,
            CallContextFieldTag::LastCalleeReturnDataLength,
            return_data_size.expr(),
        );

        // 4. memory copy
        // Construct memory address in the destination (memory) to which we copy memory.
        let dst_memory_addr = MemoryAddressGadget::construct(cb, dest_offset, size);
        // Calculate the next memory size and the gas cost for this memory
        // access. This also accounts for the dynamic gas required to copy bytes to
        // memory.
        let memory_expansion = MemoryExpansionGadget::construct(cb, [dst_memory_addr.end_offset()]);
        let memory_copier_gas = MemoryCopierGasGadget::construct(
            cb,
            dst_memory_addr.length(),
            memory_expansion.gas_cost(),
        );

        let copy_rwc_inc = cb.query_cell();
        cb.condition(dst_memory_addr.has_length(), |cb| {
            cb.copy_table_lookup(
                last_callee_id.expr(),
                CopyDataType::Memory.expr(),
                cb.curr.state.call_id.expr(),
                CopyDataType::Memory.expr(),
                return_data_offset.expr()
                    + from_bytes::expr(&check_overflow_gadget.data_offset().cells[..N_BYTES_U64]),
                return_data_offset.expr() + return_data_size.expr(),
                dst_memory_addr.offset(),
                dst_memory_addr.length(),
                0.expr(), // for RETURNDATACOPY rlc_acc is 0
                copy_rwc_inc.expr(),
            );
        });
        cb.condition(not::expr(dst_memory_addr.has_length()), |cb| {
            cb.require_zero(
                "if no bytes to copy, copy table rwc inc == 0",
                copy_rwc_inc.expr(),
            );
        });

        // State transition
        let step_state_transition = StepStateTransition {
            rw_counter: Delta(cb.rw_counter_offset()),
            program_counter: Delta(1.expr()),
            stack_pointer: Delta(3.expr()),
            gas_left: Delta(
                -(OpcodeId::RETURNDATACOPY.constant_gas_cost().expr()
                    + memory_copier_gas.gas_cost()),
            ),
            memory_word_size: To(memory_expansion.next_memory_word_size()),
            ..Default::default()
        };
        let same_context = SameContextGadget::construct(cb, opcode, step_state_transition);

        Self {
            same_context,
            last_callee_id,
            return_data_offset,
            return_data_size,
            dst_memory_addr,
            check_overflow_gadget,
            memory_expansion,
            memory_copier_gas,
            copy_rwc_inc,
        }
    }

    fn assign_exec_step(
        &self,
        region: &mut CachedRegion<'_, '_, F>,
        offset: usize,
        block: &Block,
        _tx: &Transaction,
        _call: &Call,
        step: &ExecStep,
    ) -> Result<(), Error> {
        self.same_context.assign_exec_step(region, offset, step)?;

        let [dest_offset, data_offset, size] =
            [0, 1, 2].map(|i| block.rws[step.rw_indices[i as usize]].stack_value());

        let [last_callee_id, return_data_offset, return_data_size] = [
            (3, CallContextFieldTag::LastCalleeId),
            (4, CallContextFieldTag::LastCalleeReturnDataOffset),
            (5, CallContextFieldTag::LastCalleeReturnDataLength),
        ]
        .map(|(i, tag)| {
            let rw = block.rws[step.rw_indices[i as usize]];
            assert_eq!(rw.field_tag(), Some(tag as u64));
            rw.call_context_value()
        });
        self.last_callee_id.assign(
            region,
            offset,
            Value::known(
                last_callee_id
                    .to_scalar()
                    .expect("unexpected U256 -> Scalar conversion failure"),
            ),
        )?;
        self.return_data_offset.assign(
            region,
            offset,
            Value::known(
                return_data_offset
                    .to_scalar()
                    .expect("unexpected U256 -> Scalar conversion failure"),
            ),
        )?;
        self.return_data_size.assign(
            region,
            offset,
            Value::known(
                return_data_size
                    .to_scalar()
                    .expect("unexpected U256 -> Scalar conversion failure"),
            ),
        )?;

        // assign the destination memory offset.
        let memory_address = self
            .dst_memory_addr
            .assign(region, offset, dest_offset, size)?;

        // assign to gadgets handling memory expansion cost and copying cost.
        let (_, memory_expansion_cost) = self.memory_expansion.assign(
            region,
            offset,
            step.memory_word_size(),
            [memory_address],
        )?;
        self.memory_copier_gas
            .assign(region, offset, size.as_u64(), memory_expansion_cost)?;

        self.copy_rwc_inc.assign(
            region,
            offset,
            Value::known(
                step.copy_rw_counter_delta
                    .to_scalar()
                    .expect("unexpected U256 -> Scalar conversion failure"),
            ),
        )?;

        self.check_overflow_gadget
            .assign(region, offset, data_offset, size, return_data_size)?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::{evm_circuit::test::rand_bytes, test_util::CircuitTestBuilder};
    use bus_mapping::circuit_input_builder::CircuitsParams;
    use eth_types::{bytecode, Word};
    use mock::{generate_mock_call_bytecode, test_ctx::TestContext, MockCallBytecodeParams};

    fn test_ok_internal(
        return_data_offset: usize,
        return_data_size: usize,
        size: usize,
        offset: usize,
        dest_offset: Word,
    ) {
        let (addr_a, addr_b) = (mock::MOCK_ACCOUNTS[0], mock::MOCK_ACCOUNTS[1]);

        let return_offset =
            std::cmp::max((return_data_offset + return_data_size) as i64 - 32, 0) as usize;
        let code_b = bytecode! {
            .op_mstore(return_offset, Word::from_big_endian(&rand_bytes(32)))
            .op_return(return_data_offset, return_data_size)
            STOP
        };

        // code A calls code B.
        let instruction = bytecode! {
            PUSH32(size) // size
            PUSH32(offset) // offset
            PUSH32(dest_offset) // dest_offset
            RETURNDATACOPY
        };
        let code_a = generate_mock_call_bytecode(MockCallBytecodeParams {
            address: addr_b,
            return_data_offset,
            return_data_size,
            instructions_after_call: instruction,
            ..MockCallBytecodeParams::default()
        });

        let ctx = TestContext::<3, 1>::new(
            None,
            |accs| {
                accs[0].address(addr_a).code(code_a);
                accs[1].address(addr_b).code(code_b);
                accs[2]
                    .address(mock::MOCK_ACCOUNTS[2])
                    .balance(Word::from(1u64 << 30));
            },
            |mut txs, accs| {
                txs[0].to(accs[0].address).from(accs[2].address);
            },
            |block, _tx| block,
        )
        .unwrap();

        CircuitTestBuilder::new_from_test_ctx(ctx)
            .params(CircuitsParams {
                max_rws: 2048,
                max_copy_rows: 1796,
                ..Default::default()
            })
            .run();
    }

    #[test]
    fn returndatacopy_gadget_do_nothing() {
        test_ok_internal(0, 2, 0, 0, 0x10.into());
    }

    #[test]
    fn returndatacopy_gadget_simple() {
        test_ok_internal(0, 2, 2, 0, 0x10.into());
    }

    #[test]
    fn returndatacopy_gadget_large() {
        test_ok_internal(0, 0x20, 0x20, 0, 0x20.into());
    }

    #[test]
    fn returndatacopy_gadget_large_partial() {
        test_ok_internal(0, 0x20, 0x10, 0x10, 0x20.into());
    }

    #[test]
    fn returndatacopy_gadget_zero_length() {
        test_ok_internal(0, 0, 0, 0, 0x20.into());
    }

    #[test]
    fn returndatacopy_gadget_long_length() {
        // rlc value matters only if length > 255, i.e., size.cells.len() > 1
        test_ok_internal(0, 0x200, 0x150, 0, 0x20.into());
    }

    #[test]
    fn returndatacopy_gadget_big_offset() {
        // rlc value matters only if length > 255, i.e., size.cells.len() > 1
        test_ok_internal(0x200, 0x200, 0x150, 0, 0x200.into());
    }

    #[test]
    fn returndatacopy_gadget_overflow_offset_and_zero_length() {
        test_ok_internal(0, 0x20, 0, 0x20, Word::MAX);
        test_ok_internal(0, 0x10, 0x10, 0x10, 0x20.into());
        test_ok_internal(0, 0x10, 0x10, 0, 0x2000000.into());
    }
}
