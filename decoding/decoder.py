import re

# get functions from text file in elf format text objdump
elf_func_regex = r'([0-9a-f]{8}) <(.*)>:'
instr_regex = r' ([0-9a-f]{7}):\s([0-9a-f]{4,8})\s*([^@\n]*)'

elf_dump = open('decoding/pokeemerald_modern.objdump', 'r').read()

# function_matches = re.findall(elf_func_regex, elf_dump)

instr_matches = re.findall(instr_regex, elf_dump)
print(f'total instructions: {len(instr_matches)}')

str_imm_imm5 = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b01100]
# str_imm_imm8 is not an issue because it is sp relative
str_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 9) == 0b0101000]
strb_imm = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b01110]
strb_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 9) == 0b0101010]
strh_imm = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b10000]
strh_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b0101001]

total_strs = len(str_imm_imm5) + len(str_reg) + len(strb_imm) + len(strb_reg) + len(strh_imm) + len(strh_reg)
print(f'frac of stores: {total_strs / len(instr_matches) * 100}%')

ldr_imm_imm5 = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b01100]
# ldr_imm_imm8 is not an issue because it is sp relative
ldr_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 9) == 0b0101100]
ldrb_imm = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b01111]
ldrb_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 9) == 0b0101110]
ldrh_imm = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b10001]
ldrh_reg = [match[1] for match in instr_matches if (int(match[1], base=16) >> 11) == 0b0101101]

total_ldrs = len(ldr_imm_imm5) + len(ldr_reg) + len(ldrb_imm) + len(ldrb_reg) + len(ldrh_imm) + len(ldrh_reg)
print(f'frac of loads: {total_ldrs / len(instr_matches) * 100}%')

print('done')

# functions that commonly touch VRAM
# LZ77UnCompVram