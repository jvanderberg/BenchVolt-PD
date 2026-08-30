MEMORY
{
  /* Packaged as an ordinary application image at 0x08008000 so the stock
     bootloader flashes/launches it unchanged; the bootloader itself is
     never touched. */
  FLASH : ORIGIN = 0x08008000, LENGTH = 92K

  /* Stock bootloader copies the 192-byte vector table here before jumping. */
  RAM : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
