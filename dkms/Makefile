ifneq (${KERNELRELEASE},)
ccflags-y := -I$(src)/include -DCONFIG_BCACHEFS_DKMS
obj-m += src/fs/bcachefs/
else
KDIR ?= /lib/modules/`uname -r`/build

default:
	$(MAKE) -C $(KDIR) M=$$PWD modules

clean:
	$(MAKE) -C $(KDIR) M=$$PWD clean
endif
