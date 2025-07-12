#include <stdarg.h>
#include <stdio.h>

static inline const char *real_fmt(const char *fmt)
{
	return fmt[0] == '\001' ? fmt + 2 : fmt;
}

void vprintk(const char *fmt, va_list args)
{
	vprintf(real_fmt(fmt), args);
}

void printk(const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	vprintk(fmt, args);
	va_end(args);
}
