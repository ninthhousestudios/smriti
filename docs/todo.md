heuristics for managing backup
currently, i have to put tier 2 things manually in the file.
one some level im going to need to do this, but id like to consider a higher-level way
of doing using the smriti's index. thoughts on this?

i have a usb backup drive. i want to index it also for a similar reason: most i can
probably delete. i want smriti to help me go through and decide, i.e., heuristcs...could
we use the bert model like panda to do something for this?

about the usb, its not always mounted, so i mount, and have smriti index it. does this
get stored in same db? then when i unplug the device, i can still see the index right?
this is actually fine because i only use the usb one place so i know it doesnt change.
then i plug back in and smriti will have to rescan. this works correct?

i was confused by the use of "daemon" because there are actually two daemons associated
with smriti: the mcp server and the watcher. lets talk about the watcher and sketch out
what that looks like.
