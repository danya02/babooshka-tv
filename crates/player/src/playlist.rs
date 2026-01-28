use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Playlist {
    pub items: Vec<PathBuf>,
}

impl Playlist {
    pub async fn load_from_file(path: &PathBuf) -> Result<Playlist, std::io::Error> {
        let playlist_text = tokio::fs::read_to_string(path).await?;

        let playlist: Playlist =
            serde_json::from_str(&playlist_text).map_err(std::io::Error::from)?;
        if playlist.items.is_empty() {
            panic!("playlist file should have at least one item");
        }

        Ok(playlist)
    }

    pub fn next_file(&self, current_file: &PathBuf) -> PathBuf {
        // If the current file is in the playlist, return the next one.
        if self.items.is_empty() {
            panic!("playlist is empty");
        }
        for idx in 0..self.items.len() - 1 {
            // If the current file is equal to the playlist item, return the next one.
            if self.items[idx] == *current_file {
                return self.items[idx + 1].clone();
            }
        }

        for idx in 0..self.items.len() - 1 {
            // If the current file's *filename* is equal to the playlist item, return the next one.
            if self.items[idx].file_name() == current_file.file_name() {
                return self.items[idx + 1].clone();
            }
        }

        // By default, return the first item.
        self.items[0].clone()
    }
}
