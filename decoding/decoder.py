import re

# get functions from text file in elf format text objdump
elf_func_regex = r'([0-9a-f]{8}) <(.*)>:'

test = '''
 813eadc:	1612      	asrs	r2, r2, #24
 813eade:	705c      	strb	r4, [r3, #1]
 813eae0:	7098      	strb	r0, [r3, #2]
 813eae2:	8099      	strh	r1, [r3, #4]
 813eae4:	80da      	strh	r2, [r3, #6]
 813eae6:	bc10      	pop	{r4}
 813eae8:	bc01      	pop	{r0}
 813eaea:	4700      	bx	r0
 813eaec:	02036260 	.word	0x02036260

0813eaf0 <SetWarpDestinationToDiveWarp>:
 813eaf0:	4a02      	ldr	r2, [pc, #8]	@ (813eafc <SetWarpDestinationToDiveWarp+0xc>)
 813eaf2:	4b03      	ldr	r3, [pc, #12]	@ (813eb00 <SetWarpDestinationToDiveWarp+0x10>)
 813eaf4:	ca03      	ldmia	r2!, {r0, r1}
 813eaf6:	c303      	stmia	r3!, {r0, r1}
 '''

m = re.search(elf_func_regex, test)
print(m.group(2))


# functions that commonly touch VRAM
# LZ77UnCompVram