MEMORY
{
  FLASH : ORIGIN = 0x08000000, LENGTH = 64K
  RAM   : ORIGIN = 0x20000000, LENGTH = 20K
}

ASSERT(_stack_start - _stack_end >= 4K, "STM32F103C8 requires at least 4 KiB for the stack");
