# Send/Recv

## They close send

 * current buffer can be discarded
 * future writes fail
 * stream immediately doomed
 
## They close recv
 * current buffer maintained
 * future reads continue for a while
 * stream not doomed


 * closes once all reads are complete


## We close send 
 * current buffer maintained
 * future writes fail
 * stream not doomed


 * closes once all writes are complete 
 
## We close recv
 * current buffer can be discarded
 * future reads fail
 * stream immediately doomed
