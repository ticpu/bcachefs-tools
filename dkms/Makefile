export BCACHEFS_DKMS=1
MAKEFLAGS += -j$(nproc --all)
ifneq (${KERNELRELEASE},)
ccflags-y := -I$(src)/include
obj-m += src/fs/bcachefs/
else
KDIR ?= /lib/modules/`uname -r`/build

default:
	$(MAKE) -C $(KDIR) M=$$PWD modules

clean:
	$(MAKE) -C $(KDIR) M=$$PWD clean
endif
