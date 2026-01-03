#include <stdio.h>

#include "cmds.h"
#include "version.h"

int cmd_version(int argc, char *argv[])
{
	printf("%s\n", bcachefs_version);
	return 0;
}
