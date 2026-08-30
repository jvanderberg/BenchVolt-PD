MEMORY
{
  /* v1-migration test build: the golden core packaged as an ordinary
     application image at 0x08008000 so the stock bootloader launches it. */
  FLASH : ORIGIN = 0x08008000, LENGTH = 92K
  RAM   : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
