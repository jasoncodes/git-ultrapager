use git_ultrapager::ansi::{AnsiStripper, escape_interactive_bytes};
use git_ultrapager::parser::{GitParser, parse_bytes};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn parser_is_chunk_boundary_independent(
        input in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1_usize..64,
    ) {
        let expected = parse_bytes(&input);
        let mut parser = GitParser::new();
        for chunk in input.chunks(chunk_size) {
            let _ = parser.push(chunk);
        }
        let actual = parser.into_document();
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn sanitizer_accepts_arbitrary_bytes_and_chunking(
        input in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1_usize..64,
    ) {
        let mut one_shot = AnsiStripper::default();
        let mut expected = Vec::new();
        one_shot.push(&input, &mut expected);
        one_shot.finish_into(&mut expected);

        let mut chunked = AnsiStripper::default();
        let mut actual = Vec::new();
        for chunk in input.chunks(chunk_size) {
            chunked.push(chunk, &mut actual);
        }
        chunked.finish_into(&mut actual);

        prop_assert_eq!(&actual, &expected);
        let _ = escape_interactive_bytes(&actual);
    }
}
