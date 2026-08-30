MEMORY
{
  /* One-time migrator: an ordinary v1 application image the stock
     bootloader flashes and launches (vectors copied to SRAM@0, RAM above). */
  FLASH : ORIGIN = 0x08008000, LENGTH = 92K
  RAM   : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
