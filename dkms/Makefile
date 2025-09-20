ifneq (${KERNELRELEASE},)
ccflags-y := -I$(src)/include
obj-m += src/fs/bcachefs/
else
KDIR ?= /lib/modules/`uname -r`/build

default:
	$(MAKE) -C $(KDIR) M=$$PWD BCACHEFS_DKMS=1 modules

clean:
	$(MAKE) -C $(KDIR) M=$$PWD BCACHEFS_DKMS=1 clean
endif
