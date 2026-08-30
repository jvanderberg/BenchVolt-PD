MEMORY
{
  /* Test application for the v2 layout: linked at APP_BASE, ends before the
     64-byte in-partition descriptor at 0x0801EFC0. */
  FLASH : ORIGIN = 0x08005000, LENGTH = 0x19FC0
  RAM   : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0
}
