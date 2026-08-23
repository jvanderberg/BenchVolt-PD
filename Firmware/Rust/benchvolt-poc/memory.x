MEMORY
{
  /* Existing C bootloader occupies the first 32 KiB. */
  FLASH : ORIGIN = 0x08008000, LENGTH = 92K

  /* Bootloader copies the 192-byte vector table here before jumping. */
  RAM : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
