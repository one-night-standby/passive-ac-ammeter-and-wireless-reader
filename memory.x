/* The part has 128K of MAIN flash. The last 1K sector is the field
   calibration store (src/nvcal.rs), so the linker is stopped short of it --
   this line is the only thing keeping a build from placing code where a
   calibration push will erase it. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 127K
  RAM   : ORIGIN = 0x20200000, LENGTH = 32K
}
