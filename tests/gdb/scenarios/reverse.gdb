# Forward stops are recorded on both targets; @reverse lines run only on
# SimGDB and must land on states the forward run already printed.
@cores m4 m35p m4be
break mix
continue
@dump rev_1
continue
@dump rev_2
continue
@dump rev_3
stepi
@dump rev_s1
stepi
@dump rev_s2
stepi
@dump rev_s3
@reverse reverse-stepi
@reverse @dump rev_s2
@reverse reverse-stepi
@reverse @dump rev_s1
@reverse reverse-stepi
@reverse @dump rev_3
@reverse reverse-continue
@reverse @dump rev_2
@reverse reverse-continue
@reverse @dump rev_1
