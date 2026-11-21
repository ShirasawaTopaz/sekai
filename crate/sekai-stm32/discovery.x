MEMORY
{
  FLASH : ORIGIN = 0x08000000, LENGTH = 1024K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}

ASSERT(_stack_start - _stack_end >= 4K, "STM32F407VG requires at least 4 KiB for the stack");
