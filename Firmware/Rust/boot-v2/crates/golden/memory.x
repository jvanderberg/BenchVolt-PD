MEMORY
{
  FLASH : ORIGIN = 0x08000800, LENGTH = 6K
  /* v1-compatible handover: bootloader copies its 192-byte vector table to
     SRAM@0 before jumping; RAM starts above it (contract line 13). */
  RAM   : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
