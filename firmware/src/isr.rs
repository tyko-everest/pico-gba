#[repr(C, packed)]
#[used]
#[link_section = ".interrupt_table"]
struct InterruptVectorTable {
    main_stack_pointer: u32,
    reset_handler: u32,
    nmi_handler: u32,
    hardfault_handler: u32,
    reserved_0: [u32; 7],
    supervisor_call: u32,
    reserver_1: [u32; 2],
    pendsv_handler: u32,
    systick_handler: u32,
v}
