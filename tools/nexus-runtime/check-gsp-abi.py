"""Compare native byte ABI offsets against the pinned NVIDIA C headers."""
from pathlib import Path
import json
import re
import runpy
import subprocess

REPO=Path(__file__).resolve().parents[2]
REV='ca2d03e6fafd4d4b56b383497d931c25b283bfcf'
FIELDS=[
    ('sizeof(GspFwWprMeta)',256),
    ('__builtin_offsetof(GspFwWprMeta,sysmemAddrOfRadix3Elf)',16),
    ('__builtin_offsetof(GspFwWprMeta,sizeOfRadix3Elf)',24),
    ('__builtin_offsetof(GspFwWprMeta,sysmemAddrOfBootloader)',32),
    ('__builtin_offsetof(GspFwWprMeta,sizeOfBootloader)',40),
    ('__builtin_offsetof(GspFwWprMeta,bootloaderCodeOffset)',48),
    ('__builtin_offsetof(GspFwWprMeta,bootloaderDataOffset)',56),
    ('__builtin_offsetof(GspFwWprMeta,bootloaderManifestOffset)',64),
    ('__builtin_offsetof(GspFwWprMeta,sysmemAddrOfSignature)',72),
    ('__builtin_offsetof(GspFwWprMeta,sizeOfSignature)',80),
    ('__builtin_offsetof(GspFwWprMeta,nonWprHeapSize)',104),
    ('__builtin_offsetof(GspFwWprMeta,gspFwHeapSize)',128),
    ('__builtin_offsetof(GspFwWprMeta,frtsSize)',160),
    ('__builtin_offsetof(GspFwWprMeta,vgaWorkspaceSize)',192),
    ('__builtin_offsetof(GspFwWprMeta,pmuReservedSize)',244),
    ('__builtin_offsetof(GspFwWprMeta,verified)',248),
    ('sizeof(GSP_FMC_BOOT_PARAMS)',256),
    ('__builtin_offsetof(GSP_FMC_BOOT_PARAMS,bootGspRmParams)',64),
    ('__builtin_offsetof(GSP_ACR_BOOT_GSP_RM_PARAMS,bIsGspRmBoot)',28),
    ('__builtin_offsetof(GSP_ACR_BOOT_GSP_RM_PARAMS,bIcuEnabled)',30),
    ('__builtin_offsetof(GSP_ACR_BOOT_GSP_RM_PARAMS,bScrubCbcSr)',31),
    ('__builtin_offsetof(GSP_ACR_BOOT_GSP_RM_PARAMS,gspRmDescSize)',4),
    ('__builtin_offsetof(GSP_ACR_BOOT_GSP_RM_PARAMS,gspRmDescOffset)',8),
    ('__builtin_offsetof(GSP_RM_PARAMS,bootArgsOffset)',8),
    ('__builtin_offsetof(GSP_FMC_BOOT_PARAMS,gspRmParams)',128),
    ('sizeof(NVDM_PAYLOAD_COT)',860),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,gspFmcSysmemOffset)',4),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,frtsVidmemOffset)',24),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,hash384)',36),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,publicKey)',84),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,signature)',468),
    ('__builtin_offsetof(NVDM_PAYLOAD_COT,gspBootArgsSysmemOffset)',852),
    ('sizeof(GSP_ARGUMENTS_CACHED)',184),
    ('__builtin_offsetof(GSP_ARGUMENTS_CACHED,messageQueueInitArguments)',16),
    ('__builtin_offsetof(GSP_ARGUMENTS_CACHED,bindataArgs)',152),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,sharedMemPhysAddr)',8),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,cmdQueueOffset)',16),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,statQueueOffset)',24),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,queueElementHdrSize)',32),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,queueElementSizeMin)',36),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,queueElementSizeMax)',40),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,queueHeaderAlign)',44),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,queueElementAlign)',48),
    ('__builtin_offsetof(MESSAGE_QUEUE_INIT_ARGUMENTS,pageTableEntryCount)',52),
    ('sizeof(LibosMemoryRegionInitArgument)',32),
    ('__builtin_offsetof(LibosMemoryRegionInitArgument,pa)',8),
    ('__builtin_offsetof(LibosMemoryRegionInitArgument,size)',16),
    ('__builtin_offsetof(LibosMemoryRegionInitArgument,kind)',24),
    ('__builtin_offsetof(LibosMemoryRegionInitArgument,loc)',25),
    ('LIBOS_MEMORY_REGION_CONTIGUOUS',1),
    ('LIBOS_MEMORY_REGION_LOC_SYSMEM',1),
    ('GSP_DMA_TARGET_COHERENT_SYSTEM',1),
    ('sizeof(msgqTxHeader)',32),
    ('__builtin_offsetof(msgqTxHeader,msgCount)',12),
    ('__builtin_offsetof(msgqTxHeader,entryOff)',16),
    ('sizeof(MCTP_HEADER)',7),
    ('sizeof(NVDM_PAYLOAD_COMMAND_RESPONSE)',12),
    ('__builtin_offsetof(NVDM_PAYLOAD_COMMAND_RESPONSE,commandNvdmType)',4),
    ('__builtin_offsetof(NVDM_PAYLOAD_COMMAND_RESPONSE,errorCode)',8),
    ('NVDM_TYPE_COT',0x14),
    ('NVDM_TYPE_FSP_RESPONSE',0x15),
    ('FSP_EMEM_CHANNEL_RM_SIZE',1024),
    ('MCTP_MSG_HEADER_TYPE_VENDOR_PCI',0x7e),
    ('MCTP_MSG_HEADER_VENDOR_ID_NV',0x10de),
    ('REF_NUM(MCTP_HEADER_VERSION,1) | REF_NUM(MCTP_HEADER_SOM,1) | REF_NUM(MCTP_HEADER_EOM,1)',0xc0000001),
    ('REF_DEF(MCTP_MSG_HEADER_TYPE,_VENDOR_PCI) | REF_DEF(MCTP_MSG_HEADER_VENDOR_ID,_NV) | REF_NUM(MCTP_MSG_HEADER_NVDM_TYPE,NVDM_TYPE_COT)',0x1410de7e),
    ('NV_PFSP_EMEMC(0)',0x8f2ac0),
    ('NV_PFSP_EMEMD(0)',0x8f2ac4),
    ('NV_PFSP_QUEUE_HEAD(0)',0x8f2c00),
    ('NV_PFSP_QUEUE_TAIL(0)',0x8f2c04),
    ('NV_PFSP_MSGQ_HEAD(0)',0x8f2c80),
    ('NV_PFSP_MSGQ_TAIL(0)',0x8f2c84),
    ('REF_NUM(NV_PFSP_EMEMC_AINCW,1)',1<<24),
    ('REF_NUM(NV_PFSP_EMEMC_AINCR,1)',1<<25),
]


def main():
    runpy.run_path(str(Path(__file__).with_name('fetch-nvidia.py')))
    source=REPO/'build/nvidia'/('open-gpu-kernel-modules-'+REV)
    output=REPO/'build/nvidia/abi'
    output.mkdir(parents=True,exist_ok=True)
    probe=output/'probe.c'
    probe.write_text('\n'.join([
        '#include "nvtypes.h"',
        '#include "nvmisc.h"',
        '#include "gsp/gsp_fw_wpr_meta.h"',
        '#include "gsp/gspifpub.h"',
        '#include "gpu/fsp/kern_fsp_cot_payload.h"',
        '#include "gpu/gsp/gsp_init_args.h"',
        '#include "libos_init_args.h"',
        '#include "msgq_priv.h"',
        '#include "mctp_format.h"',
        '#include "nvdm_format.h"',
        '#include "fsp/fsp_emem_channels.h"',
        '#include "fsp/nvdm_payload_cmd_response.h"',
        '#include "published/hopper/gh100/dev_fsp_pri.h"',
        'const unsigned int shape[] = {'+', '.join(field for field,_ in FIELDS)+'};',
    ])+'\n')
    command=['clang','-S','-emit-llvm','-ffreestanding','-o',str(output/'probe.ll'),str(probe)]
    for include in ['src/common/sdk/nvidia/inc','src/common/inc', 'src/nvidia/arch/nvalloc/common/inc',
                    'src/nvidia/inc/kernel','src/common/uproc/os/common/include', 'src/nvidia/inc/libraries/msgq', 'src/common/inc/swref']:
        command += ['-I',str(source/include)]
    subprocess.run(command,check=True,cwd=REPO)
    llvm=(output/'probe.ll').read_text()
    match=re.search(r'@shape\s*=.*?constant \[\d+ x i32\] \[([^\n]+)\]',llvm)
    if not match:
        raise RuntimeError('C ABI probe constants missing from LLVM output')
    values=[int(value)&0xffffffff for value in re.findall(r'i32 (-?\d+)',match[1])]
    expected=[value for _,value in FIELDS]
    if values!=expected:
        raise ValueError('Pinned NVIDIA ABI mismatch: '+str(list(zip(FIELDS,values))))
    (output/'verified.json').write_text(json.dumps(dict(zip((f for f,_ in FIELDS),values)),indent=2)+'\n')
    print('PASS:',len(values),'sizes/offsets match original pinned NVIDIA C headers; no GPU execution')


if __name__=='__main__':
    main()
