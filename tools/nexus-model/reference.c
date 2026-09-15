/* Test oracle only: compile beside the pinned upstream run.c. */
#define TESTING
#include "run.c"
int main(int argc, char **argv) {
    if (argc != 4 || strlen(argv[3]) > 1024) return 1;
    Transformer model; Tokenizer tok;
    build_transformer(&model, argv[1]);
    build_tokenizer(&tok, argv[2], model.config.vocab_size);
    int prompt[1028], n;
    encode(&tok, argv[3], 1, 0, prompt, &n);
    printf("prompt="); for(int i=0;i<n;i++) printf("%s%d", i?",":"",prompt[i]); puts("");
    int token=prompt[0], count=0;
    printf("tokens=");
    for(int pos=0;pos<model.config.seq_len && count<32;pos++) {
        float *logits=forward(&model,token,pos);
        if(pos<n-1) {token=prompt[pos+1];continue;}
        token=sample_argmax(logits,model.config.vocab_size);
        if(token==1 || token==2)break;
        printf("%s%d",count?",":"",token); count++;
    }
    puts(""); free_tokenizer(&tok); free_transformer(&model);
    return 0;
}
